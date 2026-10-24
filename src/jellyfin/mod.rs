//! Jellyfin client (plan 08): paged library reads plus the raw response for
//! the original file (`static=true`, never a server transcode).
//!
//! Credentials live **only** here (`Client.key`), and only ever leave this
//! module as the `Authorization: MediaBrowser Token="…"` header — never in a
//! URL, log line, notice or `Debug` print. Every error text is a fixed string
//! or an HTTP status code, so a key can never leak through an error either.
//!
//! Blocking `ureq` (3.x): all of this runs on worker threads (the library
//! loader, the cache downloader, the engine thread for `stream`), never on
//! the UI thread.

pub mod cache;
pub mod http_source;

use std::io::Read;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::Deserialize;

use crate::config::JellyfinConfig;
use crate::player::{Track, TrackSource};

/// Server-side page size (the plan's 500 items per request).
pub const PAGE_SIZE: usize = 500;
/// Connect + handshake budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Read budget: response headers and each body read (the plan's 15 s).
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The raw HTTP response, so callers can read `Content-Range` (streaming)
/// or stream the body straight to disk (cache downloads).
pub type Response = ureq::http::Response<ureq::Body>;

/// Messages the library-load thread posts back to the UI thread. `Page`
/// batches arrive progressively (the playlist fills while the rest of the
/// server is still being fetched); `Done` and `Error` are terminal.
#[derive(Debug)]
pub enum JfMsg {
    Page {
        tracks: Vec<Track>,
        /// Tracks handed over so far (including this page).
        loaded: usize,
        /// Server-side total (`TotalRecordCount`).
        total: usize,
    },
    Done,
    Error(String),
}

/// A Jellyfin server + API key. Cheap to clone (shared `ureq` connection
/// pool); `Clone` lets the engine hold one for `stream` mode while the UI
/// holds another.
#[derive(Clone)]
pub struct Client {
    base: String,
    key: String,
    agent: ureq::Agent,
}

impl Client {
    /// Build from `[jellyfin]`: timeouts per the plan (connect 10 s, read
    /// 15 s) and a URL whose trailing slash is already trimmed by config.
    pub fn new(cfg: &JellyfinConfig) -> Client {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(READ_TIMEOUT))
            .timeout_recv_body(Some(READ_TIMEOUT))
            .build()
            .new_agent();
        Client {
            base: cfg.url.trim_end_matches('/').to_string(),
            key: cfg.api_key.clone(),
            agent,
        }
    }

    /// The only place the key becomes a wire value: an HTTP header.
    fn auth_header(&self) -> String {
        format!("MediaBrowser Token=\"{}\"", self.key)
    }

    /// `GET {url}{path}` with the auth header, JSON-decoded. Error text is
    /// fixed — no URL, no query string, no key (plan §2).
    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth_header())
            .call()
            .map_err(map_error)?;
        let mut body = response.into_body().into_reader();
        let mut text = String::new();
        body.read_to_string(&mut text)
            .map_err(|_| "jellyfin: cannot reach server".to_string())?;
        serde_json::from_str(&text).map_err(|_| "jellyfin: bad response from server".to_string())
    }

    /// Resolve the configured `user` name (else the first user) to its id.
    pub fn user_id(&self, name: Option<&str>) -> Result<String, String> {
        let users: Vec<User> = self.get_json("/Users")?;
        if let Some(name) = name
            && let Some(found) = users.iter().find(|u| {
                u.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
        {
            return Ok(found.id.clone());
        }
        if name.is_some() {
            // A configured name that matches nobody is a config typo: do
            // not silently play as a different account.
            return Err(format!(
                "jellyfin: user \"{}\" not found",
                name.unwrap_or_default()
            ));
        }
        users
            .into_iter()
            .find(|u| !u.id.is_empty())
            .map(|u| u.id)
            .ok_or_else(|| "jellyfin: no users on the server".to_string())
    }

    /// One page of audio items, server-sorted (`Artist, SortName`), mapped
    /// to [`Track`]s. Returns the tracks plus `TotalRecordCount`.
    pub fn page(
        &self,
        user_id: &str,
        start: usize,
        limit: usize,
    ) -> Result<(Vec<Track>, usize), String> {
        let path = format!(
            "/Items?userId={user_id}&Recursive=true&IncludeItemTypes=Audio\
             &SortBy=Artist,SortName&SortOrder=Ascending&Fields=MediaSources\
             &StartIndex={start}&Limit={limit}"
        );
        let page: ItemsPage = self.get_json(&path)?;
        let total = page.total_record_count;
        let tracks = page.items.into_iter().filter_map(track_from_item).collect();
        Ok((tracks, total))
    }

    /// `GET /Audio/{id}/stream?static=true` — the original file, optionally
    /// from `range_from` bytes on (`Range: bytes=N-`). Callers inspect the
    /// status: `206` proves range support, `200` (whole body) does not.
    pub fn open_original(
        &self,
        item_id: &str,
        range_from: Option<u64>,
    ) -> Result<Response, String> {
        let url = format!("{}/Audio/{item_id}/stream?static=true", self.base);
        let mut request = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth_header());
        if let Some(from) = range_from {
            request = request.header("Range", &format!("bytes={from}-"));
        }
        request.call().map_err(map_error)
    }
}

/// Every transport/HTTP failure becomes one of three fixed strings — never
/// the error's own text (it can embed a URI, and the URI must not carry the
/// key… but we also never print a URL at all).
fn map_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => {
            "jellyfin: invalid API key".to_string()
        }
        ureq::Error::StatusCode(code) => format!("jellyfin: HTTP {code}"),
        // Connect failures, timeouts, DNS, TLS, socket errors.
        _ => "jellyfin: cannot reach server".to_string(),
    }
}

/// Fetch the whole audio library in the background, 500 items at a time,
/// posting [`JfMsg::Page`] after each page and a terminal `Done`/`Error`.
/// The receiver keeps working meanwhile — the playlist stays usable.
pub fn spawn_library_load(client: Client, user: Option<String>, tx: Sender<JfMsg>) {
    let _ = std::thread::Builder::new()
        .name("ampwire-jellyfin-load".into())
        .spawn(move || {
            let user_id = match client.user_id(user.as_deref()) {
                Ok(id) => id,
                Err(reason) => {
                    let _ = tx.send(JfMsg::Error(reason));
                    return;
                }
            };
            let mut start = 0usize;
            let mut loaded = 0usize;
            loop {
                let (tracks, total) = match client.page(&user_id, start, PAGE_SIZE) {
                    Ok(page) => page,
                    Err(reason) => {
                        let _ = tx.send(JfMsg::Error(reason));
                        return;
                    }
                };
                loaded += tracks.len();
                if tx
                    .send(JfMsg::Page {
                        tracks,
                        loaded,
                        total,
                    })
                    .is_err()
                {
                    return; // the app is gone
                }
                start += PAGE_SIZE;
                // Stop at the server's total, or on an empty page (a server
                // that keeps answering empty would loop forever otherwise).
                if start >= total {
                    break;
                }
            }
            let _ = tx.send(JfMsg::Done);
        })
        .expect("spawn the jellyfin library loader");
}

// --- wire types: PascalCase, everything optional, unknown fields ignored ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct User {
    id: String,
    name: Option<String>,
}

/// `GET /Items` page body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsPage {
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    total_record_count: usize,
}

/// One audio item. Field names follow the Jellyfin API's PascalCase; every
/// field is optional so a sparse item degrades instead of failing the page.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Item {
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    artists: Option<Vec<String>>,
    album_artist: Option<String>,
    run_time_ticks: Option<i64>,
    container: Option<String>,
    #[serde(default)]
    media_sources: Option<Vec<MediaSource>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaSource {
    bitrate: Option<u64>,
    container: Option<String>,
}

/// File extension for a container name (`"m4a"` → `"m4a"`, a comma list →
/// its first segment, junk/missing → `"bin"`): ASCII alphanumerics only, so
/// a hostile server string can never escape the cache directory.
pub(crate) fn container_ext(container: Option<&str>) -> String {
    let cleaned: String = container
        .unwrap_or("bin")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() {
        "bin".into()
    } else {
        cleaned.to_lowercase()
    }
}

/// Map one server item to a [`Track`]; `None` when the item cannot be
/// played or identified (no `Id` / `Name`).
fn track_from_item(item: Item) -> Option<Track> {
    let item_id = item.id?.trim().to_string();
    if item_id.is_empty() {
        return None;
    }
    let title = item.name?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    // `Artists[0]`, else `AlbumArtist`, else no artist line.
    let artist = item
        .artists
        .and_then(|artists| {
            artists
                .into_iter()
                .find(|name| !name.trim().is_empty())
                .map(|name| name.trim().to_string())
        })
        .or_else(|| {
            item.album_artist
                .filter(|name| !name.trim().is_empty())
                .map(|name| name.trim().to_string())
        });

    // `RunTimeTicks` is in 100 ns ticks (÷ 10_000_000 = seconds).
    let duration = item
        .run_time_ticks
        .filter(|ticks| *ticks > 0)
        .map(|ticks| Duration::from_secs_f64(ticks as f64 / 10_000_000.0));

    // The media source's container (`m4a`, `flac`, …) is a usable file
    // extension; the item-level one is a comma list (`mov,mp4,m4a,…`), of
    // which only the first segment can stand in.
    let media = item
        .media_sources
        .and_then(|sources| sources.into_iter().next());
    let container = media
        .as_ref()
        .and_then(|source| source.container.clone())
        .or_else(|| {
            item.container
                .as_deref()
                .map(|list| list.split(',').next().unwrap_or("").trim().to_string())
        })
        .filter(|c| !c.is_empty());

    // `MediaSources[0].Bitrate` is bits/s; the format line wants kbps.
    let bitrate_kbps = media
        .and_then(|source| source.bitrate)
        .map(|bps| (bps / 1000) as u32)
        .filter(|kbps| *kbps > 0);

    Some(Track {
        id: Track::next_id(),
        source: TrackSource::Remote { item_id, container },
        artist,
        title: Some(title),
        duration,
        bitrate_kbps,
    })
}
