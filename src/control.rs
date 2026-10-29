//! Unix control socket: `ampwire --status`, `ampwire --ctl …`, and the
//! single-instance guard.
//!
//! One owner-only socket (`$XDG_RUNTIME_DIR/ampwire/control.sock`, or
//! `~/.cache/ampwire/control.sock`) carries newline-delimited JSON. The
//! listener only reads the shared [`View`] and forwards [`RemoteCommand`]s to
//! the UI thread; it never touches the player or the audio engine.
//!
//! A second launch connects before doing any audio work, forwards its paths
//! (`{"cmd":"open",…}`), prints a line and exits — so two copies can never
//! fight over the sound card.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::remote::{RemoteCommand, SharedView, View, read_view};
use crate::smb::{LoadInput, parse_load_input};

/// Socket I/O must never hang the listener thread (or a `--status` client).
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// A status/control request is tiny; refuse anything larger.
const MAX_REQUEST: usize = 64 * 1024;

/// What `start` did. `AlreadyRunning` means the paths were forwarded.
pub enum Start {
    Server(ControlServer),
    AlreadyRunning,
    Off,
}

/// The listening socket and its accept thread. Dropping it stops the thread
/// and removes the socket file (a crash leaves a stale file that the next
/// start replaces).
pub struct ControlServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The accept is blocking: one throwaway connection wakes it so it
        // sees `stop` (the Release store pairs with the loop's Acquire
        // load). If that connect fails (the socket file is gone) the
        // thread is left to exit with the process rather than joined forever.
        let woke = UnixStream::connect(&self.path).is_ok();
        if let Some(thread) = self.thread.take()
            && woke
        {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
enum Request {
    Status,
    Ctl {
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs: Vec<String>,
    },
    Open {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs: Vec<String>,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    view: Option<View>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            view: None,
            error: None,
        }
    }

    fn with_view(view: View) -> Self {
        Self {
            ok: true,
            view: Some(view),
            error: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            view: None,
            error: Some(message.into()),
        }
    }
}

/// Where the control socket lives. `AMPWIRE_CONTROL_SOCKET` overrides it
/// (tests, unusual setups); otherwise the per-user runtime dir, then
/// `~/.cache` on macOS/Unix systems without `XDG_RUNTIME_DIR`.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AMPWIRE_CONTROL_SOCKET") {
        return Some(PathBuf::from(path));
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir.join("ampwire").join("control.sock"));
        }
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".cache")
                .join("ampwire")
                .join("control.sock")
        })
}

/// Try to become the single running session. On `AlreadyRunning`, the
/// caller should exit: `start` has already forwarded `inputs` to the owner.
///
/// `enabled` is false for `--demo` (a demo must not hijack a real session).
pub fn start(
    view: SharedView,
    tx: Sender<RemoteCommand>,
    inputs: &[LoadInput],
    enabled: bool,
) -> Start {
    if !enabled || std::env::var_os("AMPWIRE_NO_CONTROL").is_some() {
        return Start::Off;
    }
    let Some(path) = socket_path() else {
        return Start::Off;
    };
    if let Err(err) = ensure_parent(&path) {
        eprintln!("ampwire: control socket: {err}");
        return Start::Off;
    }

    // A live owner answers immediately; a stale socket refuses the connect.
    match UnixStream::connect(&path) {
        Ok(mut stream) => {
            let _ = configure(&stream);
            if let Err(err) = forward_open(&mut stream, inputs) {
                eprintln!("ampwire: already running (control: {err})");
            }
            return Start::AlreadyRunning;
        }
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(err) => {
            eprintln!("ampwire: control socket: {err}");
            return Start::Off;
        }
    }

    match bind(&path, view.clone(), tx.clone()) {
        Ok(server) => Start::Server(server),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            // Another instance won the bind race. Forward to it instead.
            if let Ok(mut stream) = UnixStream::connect(&path) {
                let _ = configure(&stream);
                if let Err(err) = forward_open(&mut stream, inputs) {
                    eprintln!("ampwire: already running (control: {err})");
                }
                return Start::AlreadyRunning;
            }
            if let Err(err) = remove_stale_socket(&path) {
                eprintln!("ampwire: control socket: {err}");
                return Start::Off;
            }
            match bind(&path, view, tx) {
                Ok(server) => Start::Server(server),
                Err(err) => {
                    eprintln!("ampwire: control socket: {err}");
                    Start::Off
                }
            }
        }
        Err(err) => {
            eprintln!("ampwire: control socket: {err}");
            Start::Off
        }
    }
}

/// Bind without the single-instance dance — used by the tests.
#[cfg(test)]
fn start_at(path: &Path, view: SharedView, tx: Sender<RemoteCommand>) -> io::Result<ControlServer> {
    ensure_parent(path)?;
    let _ = fs::remove_file(path);
    bind(path, view, tx)
}

fn bind(path: &Path, view: SharedView, tx: Sender<RemoteCommand>) -> io::Result<ControlServer> {
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || accept_loop(&listener, &thread_stop, &view, &tx));
    Ok(ControlServer {
        path: path.to_path_buf(),
        stop,
        thread: Some(thread),
    })
}

fn accept_loop(
    listener: &UnixListener,
    stop: &AtomicBool,
    view: &SharedView,
    tx: &Sender<RemoteCommand>,
) {
    // Blocks in `accept` (no polling): an idle session costs no wake-ups.
    // `ControlServer::drop` ends the loop with one connection of its own.
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = handle_client(stream, view, tx);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn handle_client(
    mut stream: UnixStream,
    view: &SharedView,
    tx: &Sender<RemoteCommand>,
) -> io::Result<()> {
    configure(&stream)?;
    let mut reader = BufReader::new(stream.try_clone()?).take((MAX_REQUEST + 1) as u64);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.len() > MAX_REQUEST {
        return write_response(&mut stream, &Response::failed("request too large"));
    }
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(err) => {
            return write_response(
                &mut stream,
                &Response::failed(format!("bad request: {err}")),
            );
        }
    };
    let response = match request {
        Request::Status => Response::with_view(read_view(view)),
        Request::Ping => Response::ok(),
        Request::Ctl {
            action,
            value,
            inputs,
        } => match apply_ctl(view, tx, &action, value.as_deref(), &inputs) {
            Ok(()) => Response::ok(),
            Err(err) => Response::failed(err),
        },
        Request::Open { inputs } => match open_inputs(tx, &inputs) {
            Ok(()) => Response::ok(),
            Err(err) => Response::failed(err),
        },
    };
    write_response(&mut stream, &response)
}

fn write_response(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let mut body = serde_json::to_vec(response).map_err(io::Error::other)?;
    body.push(b'\n');
    stream.write_all(&body)
}

fn configure(stream: &UnixStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))
}

fn forward_open(stream: &mut UnixStream, inputs: &[LoadInput]) -> Result<(), String> {
    let response = request_on(
        stream,
        &Request::Open {
            inputs: inputs.iter().map(input_arg).collect(),
        },
    )?;
    if response.ok {
        Ok(())
    } else {
        Err(response.error.unwrap_or_else(|| "request failed".into()))
    }
}

fn open_inputs(tx: &Sender<RemoteCommand>, inputs: &[String]) -> Result<(), String> {
    if inputs.is_empty() {
        return Ok(());
    }
    let parsed = parse_inputs(inputs)?;
    tx.send(RemoteCommand::Load(parsed))
        .map_err(|_| "ampwire is shutting down".into())
}

fn apply_ctl(
    view: &SharedView,
    tx: &Sender<RemoteCommand>,
    action: &str,
    value: Option<&str>,
    inputs: &[String],
) -> Result<(), String> {
    let command = match action {
        "play" => RemoteCommand::Play,
        "pause" => RemoteCommand::Pause,
        "toggle" | "play-pause" | "playpause" => RemoteCommand::TogglePlayPause,
        "stop" => RemoteCommand::Stop,
        "next" => RemoteCommand::Next,
        "prev" | "previous" => RemoteCommand::Prev,
        "shuffle" => RemoteCommand::ToggleShuffle,
        "repeat" => RemoteCommand::ToggleRepeat,
        "quit" => RemoteCommand::Quit,
        "seek" => {
            let seconds = parse_seconds(value)?;
            RemoteCommand::SeekBy {
                delta_ms: seconds_to_ms(seconds),
            }
        }
        "seek-to" => {
            let seconds = parse_seconds(value)?.max(0.0);
            let track_id = read_view(view)
                .track
                .map(|track| track.id)
                .ok_or_else(|| "nothing playing".to_string())?;
            RemoteCommand::SeekTo {
                track_id,
                position_ms: seconds_to_ms(seconds).max(0) as u64,
            }
        }
        "volume" => {
            let volume = value
                .ok_or_else(|| "volume needs 0-100".to_string())?
                .parse::<u8>()
                .map_err(|_| "volume needs a number 0-100".to_string())?;
            RemoteCommand::SetVolume(volume.min(100))
        }
        "load" => {
            let mut all = inputs.to_vec();
            if let Some(value) = value {
                all.push(value.to_string());
            }
            if all.is_empty() {
                return Err("load needs a path or smb:// URL".into());
            }
            RemoteCommand::Load(parse_inputs(&all)?)
        }
        other => return Err(format!("unknown control action: {other}")),
    };
    tx.send(command)
        .map_err(|_| "ampwire is shutting down".into())
}

fn parse_inputs(inputs: &[String]) -> Result<Vec<LoadInput>, String> {
    inputs
        .iter()
        .map(|text| parse_load_input(text))
        .collect::<Result<Vec<_>, _>>()
}

fn parse_seconds(value: Option<&str>) -> Result<f64, String> {
    value
        .ok_or_else(|| "seek needs a number of seconds".to_string())?
        .parse::<f64>()
        .map_err(|_| "seek needs a number of seconds".to_string())
}

fn seconds_to_ms(seconds: f64) -> i64 {
    if !seconds.is_finite() {
        return 0;
    }
    (seconds * 1000.0).clamp(i64::MIN as f64 / 2.0, i64::MAX as f64 / 2.0) as i64
}

fn input_arg(input: &LoadInput) -> String {
    match input {
        LoadInput::Local(path) => resolve_local_arg(&path.to_string_lossy()),
        LoadInput::Smb(url) => url.canonical(),
    }
}

/// Resolve a client-side relative path before it crosses the socket. The
/// running session has its own working directory, so a bare `Music` must not
/// be forwarded and resolved there.
fn resolve_local_arg(text: &str) -> String {
    if text
        .get(..4)
        .is_some_and(|head| head.eq_ignore_ascii_case("smb:"))
    {
        return text.to_string();
    }
    let path = Path::new(text);
    let resolved = if let Some(rest) = text.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map_or_else(|| path.to_path_buf(), |home| PathBuf::from(home).join(rest))
    } else if text == "~" {
        std::env::var_os("HOME").map_or_else(|| path.to_path_buf(), PathBuf::from)
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    resolved.to_string_lossy().into_owned()
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    let existed = parent.exists();
    fs::create_dir_all(parent)?;
    if !existed {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn request_on(stream: &mut UnixStream, request: &Request) -> Result<Response, String> {
    let mut body = serde_json::to_vec(request).map_err(|err| err.to_string())?;
    body.push(b'\n');
    stream
        .write_all(&body)
        .map_err(|err| format!("write: {err}"))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|err| format!("read: {err}"))?;
    if line.is_empty() {
        return Err("no response".into());
    }
    serde_json::from_str(line.trim()).map_err(|err| format!("bad response: {err}"))
}

/// `ampwire --status [--json]`. Returns the process exit code.
pub fn client_status(json: bool) -> i32 {
    let Some(path) = socket_path() else {
        return print_not_running(json);
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.kind() == io::ErrorKind::ConnectionRefused =>
        {
            return print_not_running(json);
        }
        Err(err) => {
            eprintln!("ampwire: control socket: {err}");
            return 1;
        }
    };
    if let Err(err) = configure(&stream) {
        eprintln!("ampwire: control socket: {err}");
        return 1;
    }
    let response = match request_on(&mut stream, &Request::Status) {
        Ok(response) => response,
        Err(err) => {
            eprintln!("ampwire: status: {err}");
            return 1;
        }
    };
    if !response.ok {
        eprintln!(
            "ampwire: status: {}",
            response.error.unwrap_or_else(|| "request failed".into())
        );
        return 1;
    }
    let Some(view) = response.view else {
        eprintln!("ampwire: status: empty response");
        return 1;
    };
    if json {
        match serde_json::to_string_pretty(&view) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("ampwire: status: {err}");
                return 1;
            }
        }
    } else {
        print_human(&view);
    }
    i32::from(!view.running)
}

/// `ampwire --ctl <command> [value]`. Returns the process exit code.
pub fn client_ctl(args: &[String]) -> i32 {
    let Some(action) = args.first() else {
        eprintln!("ampwire: --ctl needs a command (play, pause, next, …)");
        return 2;
    };
    let action = action.to_ascii_lowercase();
    let value = if action == "load" {
        None
    } else {
        args.get(1).cloned()
    };
    let inputs = if action == "load" {
        args.iter()
            .skip(1)
            .map(|arg| resolve_local_arg(arg))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if matches!(action.as_str(), "seek" | "seek-to" | "volume" | "load") && args.len() < 2 {
        eprintln!("ampwire: --ctl {action} needs a value");
        return 2;
    }

    let Some(path) = socket_path() else {
        eprintln!("ampwire: not running");
        return 1;
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.kind() == io::ErrorKind::ConnectionRefused =>
        {
            eprintln!("ampwire: not running");
            return 1;
        }
        Err(err) => {
            eprintln!("ampwire: control socket: {err}");
            return 1;
        }
    };
    if let Err(err) = configure(&stream) {
        eprintln!("ampwire: control socket: {err}");
        return 1;
    }
    let request = Request::Ctl {
        action: action.clone(),
        value,
        inputs,
    };
    match request_on(&mut stream, &request) {
        Ok(response) if response.ok => {
            println!("ok: {action}");
            0
        }
        Ok(response) => {
            eprintln!(
                "ampwire: {}",
                response.error.unwrap_or_else(|| "request failed".into())
            );
            1
        }
        Err(err) => {
            eprintln!("ampwire: {err}");
            1
        }
    }
}

fn print_not_running(json: bool) -> i32 {
    let view = View::not_running();
    if json {
        if let Ok(text) = serde_json::to_string_pretty(&view) {
            println!("{text}");
        }
    } else {
        println!("ampwire: not running");
    }
    1
}

fn print_human(view: &View) {
    if !view.running {
        println!("ampwire: not running");
        return;
    }
    println!("ampwire {} · {}", view.version, view.state);
    if let Some(track) = &view.track {
        if track.artist.is_empty() {
            println!("{}", clean(&track.title));
        } else {
            println!("{} — {}", clean(&track.artist), clean(&track.title));
        }
    }
    if view.track.is_some() && !view.format.codec.is_empty() {
        println!("{}", format_line(&view.format));
    }
    let duration = view.track.as_ref().map_or(0, |track| track.duration_ms);
    let position = mmss(view.position_ms);
    let total = if duration == 0 {
        "--:--".to_string()
    } else {
        mmss(duration)
    };
    let playlist = view.playlist.index.map_or_else(
        || format!("0/{}", view.playlist.length),
        |index| format!("{}/{}", index + 1, view.playlist.length),
    );
    println!(
        "{position} / {total} · volume {} · shuffle {} · repeat {} · playlist {playlist}",
        view.volume,
        on_off(view.shuffle),
        on_off(view.repeat)
    );
    if let Some(notice) = &view.notice {
        println!("notice: {}", clean(notice));
    }
}

fn format_line(format: &crate::remote::FormatView) -> String {
    use std::fmt::Write as _;
    // Writing into a `String` cannot fail, so the results are dropped.
    let mut text = format.codec.clone();
    if format.lossless {
        match (format.bit_depth, format.sample_rate) {
            (Some(depth), Some(rate)) => {
                let _ = write!(
                    text,
                    " {depth}/{}/{}",
                    crate::player::format_khz(rate),
                    format.channels
                );
            }
            (Some(depth), None) => {
                let _ = write!(text, " {depth}-bit {}", format.channels);
            }
            (None, Some(rate)) => {
                let _ = write!(
                    text,
                    " {}/{}",
                    crate::player::format_khz(rate),
                    format.channels
                );
            }
            (None, None) => {
                let _ = write!(text, " {}", format.channels);
            }
        }
    } else {
        if let Some(kbps) = format.bitrate_kbps {
            let _ = write!(text, " {kbps}kbps");
        }
        if let Some(rate) = format.sample_rate {
            let _ = write!(text, " {}kHz", crate::player::format_khz(rate));
        }
        let _ = write!(text, " {}", format.channels);
    }
    if format.bit_perfect {
        text.push_str(" · bit-perfect (app)");
    }
    text.trim().to_string()
}

fn mmss(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Tags and notices can carry control characters; never forward terminal
/// escapes to the user's shell.
fn clean(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::player::mock::MockPlayer;
    use crate::remote::{View, new_view, write_view};

    fn running_view() -> SharedView {
        let view = new_view();
        write_view(
            &view,
            View::from_snapshot(&MockPlayer::default().snapshot(), Some("test")),
        );
        view
    }

    fn connect(path: &Path) -> UnixStream {
        let stream = UnixStream::connect(path).expect("connect");
        configure(&stream).expect("configure");
        stream
    }

    #[test]
    fn relative_paths_are_resolved_before_forwarding() {
        assert_eq!(resolve_local_arg("/tmp/music"), "/tmp/music");
        assert_eq!(resolve_local_arg("smb://nas/Music"), "smb://nas/Music");
        let relative = resolve_local_arg("Music");
        assert!(Path::new(&relative).is_absolute(), "{relative}");
    }

    #[test]
    fn status_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let (tx, _rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();

        let mut stream = connect(&path);
        let response = request_on(&mut stream, &Request::Status).unwrap();
        assert!(response.ok);
        let view = response.view.unwrap();
        assert!(view.running);
        assert_eq!(view.state, "playing");
        assert_eq!(view.notice.as_deref(), Some("test"));
        drop(server);
    }

    #[test]
    fn ctl_round_trip_sends_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let (tx, rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();

        let mut stream = connect(&path);
        let request = Request::Ctl {
            action: "next".into(),
            value: None,
            inputs: Vec::new(),
        };
        let response = request_on(&mut stream, &request).unwrap();
        assert!(response.ok);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Ok(RemoteCommand::Next)
        ));
        drop(server);
    }

    #[test]
    fn open_round_trip_forwards_a_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let (tx, rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();

        let mut stream = connect(&path);
        let request = Request::Open {
            inputs: vec!["/tmp/music".into()],
        };
        let response = request_on(&mut stream, &request).unwrap();
        assert!(response.ok);
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            RemoteCommand::Load(inputs) => {
                assert!(
                    matches!(inputs.as_slice(), [LoadInput::Local(path)] if path == Path::new("/tmp/music"))
                );
            }
            other => panic!("expected Load, got {other:?}"),
        }
        drop(server);
    }

    #[test]
    fn unknown_action_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let (tx, _rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();

        let mut stream = connect(&path);
        let request = Request::Ctl {
            action: "dance".into(),
            value: None,
            inputs: Vec::new(),
        };
        let response = request_on(&mut stream, &request).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("unknown control action"));
        drop(server);
    }

    #[test]
    fn drop_removes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let (tx, _rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();
        assert!(path.exists());
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn socket_permissions_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let (tx, _rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(server);
    }

    #[test]
    fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        // Bind and drop leaves the stale file behind.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (tx, _rx) = mpsc::channel();
        let server = start_at(&path, running_view(), tx).unwrap();
        assert!(path.exists());
        drop(server);
    }
}
