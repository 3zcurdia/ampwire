//! A tiny Jellyfin stand-in for plan 08's tests (step 7): real HTTP over a
//! `TcpListener` on `127.0.0.1:0`, real `Range` support over the bytes of
//! `tests/fixtures/tone.flac`, plus a request log so tests can prove that
//! the key rides **only** in the `Authorization` header — never in a
//! request line — and that every request carries it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{JellyfinConfig, JellyfinMode};
use crate::jellyfin::Client;

/// The (fake) API key every test authenticates with.
pub const KEY: &str = "testkey-0123456789abcdef";

/// Two library items: the first has no `Artists` (falls back to
/// `AlbumArtist`), carries the album-position fields (`Album`,
/// `IndexNumber`, `ParentIndexNumber`, plan 12) and an unknown extra
/// field; the second has no `RunTimeTicks` (no duration).
/// `TotalRecordCount` outgrows one page, so the loader really pages
/// (`Page, Page, Done`).
const ITEMS_PAGE: &str = r#"{"Items":[{"Id":"item1","Name":"Song One","AlbumArtist":"The Album Artist","Album":"The Album","IndexNumber":3,"ParentIndexNumber":2,"RunTimeTicks":2500000000,"Container":"mp3","MediaSources":[{"Bitrate":320000,"Container":"mp3"}],"SomethingUnknown":{"nested":[1,2],"flag":true}},{"Id":"item2","Name":"Song Two","Artists":["Artist Two"],"Container":"flac","MediaSources":[{"Bitrate":1000000,"Container":"flac"}]}],"TotalRecordCount":600}"#;
/// What later pages return (the loader stops on the empty page).
const ITEMS_EMPTY: &str = r#"{"Items":[],"TotalRecordCount":600}"#;
const USERS: &str = r#"[{"Name":"tester","Id":"user1"}]"#;

/// One logged request: the request line as sent, and the `Authorization`
/// header value (if any).
#[derive(Debug, Clone)]
pub struct Logged {
    pub line: String,
    pub auth: Option<String>,
}

/// Shared behaviour switches (settable while a test runs).
#[derive(Clone)]
struct State {
    log: Arc<Mutex<Vec<Logged>>>,
    auth_fail: Arc<AtomicBool>,
    no_ranges: Arc<AtomicBool>,
    audio_delay: Arc<Mutex<Duration>>,
    /// Overrides every page's `TotalRecordCount` (a server that lies).
    items_total: Arc<Mutex<Option<usize>>>,
    /// Trailing JSON whitespace appended to every `/Items` body.
    items_padding: Arc<Mutex<usize>>,
}

/// The stand-in server. One (detached) accept thread, one thread per
/// connection; it lives for the whole test process.
pub struct MockServer {
    /// `http://127.0.0.1:<port>` — drop straight into a [`MockServer::client`].
    pub base: String,
    state: State,
}

impl MockServer {
    /// Bind `127.0.0.1:0` and start serving.
    pub fn start() -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock server");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = State {
            log: Arc::new(Mutex::new(Vec::new())),
            auth_fail: Arc::new(AtomicBool::new(false)),
            no_ranges: Arc::new(AtomicBool::new(false)),
            audio_delay: Arc::new(Mutex::new(Duration::ZERO)),
            items_total: Arc::new(Mutex::new(None)),
            items_padding: Arc::new(Mutex::new(0)),
        };
        let fixture = Arc::new(
            std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"),
            )
            .expect("tone.flac fixture"),
        );
        let accept_state = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let state = accept_state.clone();
                let fixture = Arc::clone(&fixture);
                std::thread::spawn(move || {
                    let _ = serve(stream, state, fixture);
                });
            }
        });
        MockServer { base, state }
    }

    /// A [`Client`] pointed at this server with `key`.
    pub fn client(&self, key: &str) -> Client {
        Client::new(&JellyfinConfig {
            url: self.base.clone(),
            api_key: key.to_string(),
            api_key_file: None,
            user: None,
            mode: JellyfinMode::Cache,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
            notice: None,
        })
    }

    /// Every request seen so far, in arrival order.
    pub fn log(&self) -> Vec<Logged> {
        self.state.log.lock().expect("log lock").clone()
    }

    /// Answer `401 Unauthorized` to everything (a revoked/typo'd key).
    pub fn set_auth_fail(&self, on: bool) {
        self.state.auth_fail.store(on, Ordering::Relaxed);
    }

    /// Ignore `Range` and always answer `200` with the whole body — the
    /// server shape that triggers plan 08's cache fallback.
    pub fn set_no_ranges(&self, on: bool) {
        self.state.no_ranges.store(on, Ordering::Relaxed);
    }

    /// Report `total` as every page's `TotalRecordCount`, whatever the
    /// pages really hold.
    pub fn set_items_total(&self, total: usize) {
        *self.state.items_total.lock().expect("total lock") = Some(total);
    }

    /// Pad every `/Items` body with `bytes` of (valid) trailing whitespace.
    pub fn set_items_padding(&self, bytes: usize) {
        *self.state.items_padding.lock().expect("padding lock") = bytes;
    }

    /// Pause between the audio headers and its body, so a test can observe
    /// a download in flight (its `.part` file).
    pub fn set_audio_delay(&self, delay: Duration) {
        *self.state.audio_delay.lock().expect("delay lock") = delay;
    }
}

/// Read one request head (GET has no body) and answer it.
fn serve(mut stream: TcpStream, state: State, fixture: Arc<Vec<u8>>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        let read = stream.read(&mut buf)?;
        if read == 0 {
            return Ok(()); // went away before a full head
        }
        head.extend_from_slice(&buf[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return Ok(());
        }
    }

    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers: HashMap<String, String> = lines
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.trim().to_lowercase(), value.trim().to_string()))
        })
        .collect();

    state.log.lock().expect("log lock").push(Logged {
        line: request_line.clone(),
        auth: headers.get("authorization").cloned(),
    });

    if state.auth_fail.load(Ordering::Relaxed) {
        return send(&mut stream, "401 Unauthorized", &[], Duration::ZERO, b"");
    }

    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default();
    match path {
        "/Users" => send(
            &mut stream,
            "200 OK",
            &[("Content-Type", "application/json")],
            Duration::ZERO,
            USERS.as_bytes(),
        ),
        "/Items" => {
            // Only `StartIndex=0` carries the two items; later pages are
            // empty (but keep the same total, so paging really happens).
            let mut body = if target.contains("StartIndex=0") {
                ITEMS_PAGE
            } else {
                ITEMS_EMPTY
            }
            .to_string();
            if let Some(total) = *state.items_total.lock().expect("total lock") {
                body = body.replace(
                    "\"TotalRecordCount\":600",
                    &format!("\"TotalRecordCount\":{total}"),
                );
            }
            let padding = *state.items_padding.lock().expect("padding lock");
            body.push_str(&" ".repeat(padding));
            send(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                Duration::ZERO,
                body.as_bytes(),
            )
        }
        _ if path.starts_with("/Audio/") && path.ends_with("/stream") => {
            send_audio(&mut stream, &state, &headers, &fixture)
        }
        _ => send(&mut stream, "404 Not Found", &[], Duration::ZERO, b""),
    }
}

/// The original file, honouring `Range` unless the test switched that off.
fn send_audio(
    stream: &mut TcpStream,
    state: &State,
    headers: &HashMap<String, String>,
    fixture: &[u8],
) -> std::io::Result<()> {
    let delay = *state.audio_delay.lock().expect("delay lock");
    let range = headers.get("range");
    if !state.no_ranges.load(Ordering::Relaxed)
        && let Some(range) = range
    {
        let Some((start, end)) = parse_range(range, fixture.len()) else {
            // A cursor at/past the end: the honest 416.
            let content_range = format!("bytes */{}", fixture.len());
            return send(
                stream,
                "416 Range Not Satisfiable",
                &[("Content-Range", content_range.as_str())],
                Duration::ZERO,
                b"",
            );
        };
        let content_range = format!("bytes {start}-{end}/{}", fixture.len());
        return send(
            stream,
            "206 Partial Content",
            &[
                ("Content-Type", "audio/flac"),
                ("Accept-Ranges", "bytes"),
                ("Content-Range", content_range.as_str()),
            ],
            delay,
            &fixture[start..=end],
        );
    }
    send(
        stream,
        "200 OK",
        &[("Content-Type", "audio/flac"), ("Accept-Ranges", "bytes")],
        delay,
        fixture,
    )
}

/// `bytes=0-`, `bytes=100-199`, `bytes=-65536` → inclusive `(start, end)`.
/// `None` when the start is past the end (→ 416).
fn parse_range(header: &str, len: usize) -> Option<(usize, usize)> {
    let spec = header.strip_prefix("bytes=")?;
    let (from, to) = spec.split_once('-')?;
    let start = if from.is_empty() {
        // suffix range: last N bytes
        let suffix: usize = to.parse().ok()?;
        return len.checked_sub(suffix).map(|start| (start, len - 1));
    } else {
        from.parse::<usize>().ok()?
    };
    if start >= len {
        return None;
    }
    let end = if to.is_empty() {
        len - 1
    } else {
        to.parse::<usize>().ok()?.min(len - 1)
    };
    Some((start, end.max(start)))
}

/// One response: headers, then (after `delay`) the body. `Connection:
/// close`, so every request gets its own connection — no keep-alive
/// bookkeeping to get wrong in a test.
fn send(
    stream: &mut TcpStream,
    status: &str,
    extra: &[(&str, &str)],
    delay: Duration,
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (key, value) in extra {
        head.push_str(key);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
    stream.write_all(body)?;
    stream.flush()
}
