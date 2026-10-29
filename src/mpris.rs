//! Linux MPRIS2 service.
//!
//! Exposes `org.mpris.MediaPlayer2.ampwire` so Omarchy's media widget, the
//! lock screen, hardware media keys and `playerctl` can see and control the
//! running session. The UI thread remains the only owner of the player: this
//! module reads the shared [`View`] and sends [`RemoteCommand`]s over the
//! same channel as the control socket.
//!
//! Nothing here is on the audio path. A failed session-bus connection (SSH,
//! a broken bus) simply disables MPRIS; the player keeps working.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use zbus::blocking::{Connection, connection};
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::remote::{RemoteCommand, SharedView, View, read_view};

const BUS_NAME: &str = "org.mpris.MediaPlayer2.ampwire";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const TRACK_PATH_PREFIX: &str = "/org/ampwire/track/";
/// How often the view is compared and `PropertiesChanged`/`Seeked` emitted.
const POLL: Duration = Duration::from_millis(200);
/// A position jump larger than this is a seek, not playback progress.
const SEEK_THRESHOLD_MS: i64 = 2_000;

/// Owns the D-Bus connection and the two helper threads. Dropping it releases
/// the bus name immediately.
pub struct MprisHandle {
    stop: Arc<AtomicBool>,
    conn: Connection,
    closed: Option<JoinHandle<()>>,
    emitter: Option<JoinHandle<()>>,
}

impl Drop for MprisHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.conn.clone().close();
        if let Some(thread) = self.closed.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.emitter.take() {
            let _ = thread.join();
        }
    }
}

/// Publish the MPRIS service. `None` when there is no usable session bus or
/// another process already owns the name (the player still works).
pub fn start(view: SharedView, tx: Sender<RemoteCommand>) -> Option<MprisHandle> {
    let root = RootIface { tx: tx.clone() };
    let player = PlayerIface {
        tx,
        view: view.clone(),
    };
    let conn = connection::Builder::session()
        .ok()?
        .name(BUS_NAME)
        .ok()?
        .serve_at(OBJECT_PATH, root)
        .ok()?
        .serve_at(OBJECT_PATH, player)
        .ok()?
        .build()
        .ok()?;

    let stop = Arc::new(AtomicBool::new(false));
    let closed_conn = conn.clone();
    let wait = closed_conn.clone();
    let closed = thread::spawn(move || wait.closed());
    let emitter_stop = Arc::clone(&stop);
    let emitter = thread::spawn(move || emitter_loop(conn, view, emitter_stop));
    Some(MprisHandle {
        stop,
        conn: closed_conn,
        closed: Some(closed),
        emitter: Some(emitter),
    })
}

struct RootIface {
    tx: Sender<RemoteCommand>,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl RootIface {
    #[zbus(property(emits_changed_signal = "false"))]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn identity(&self) -> String {
        "Ampwire".into()
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn desktop_entry(&self) -> String {
        "ampwire".into()
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_set_fullscreen(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn fullscreen(&self) -> bool {
        false
    }

    fn raise(&self) {}

    fn quit(&self) {
        let _ = self.tx.send(RemoteCommand::Quit);
    }
}

struct PlayerIface {
    tx: Sender<RemoteCommand>,
    view: SharedView,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl PlayerIface {
    fn next(&self) {
        let _ = self.tx.send(RemoteCommand::Next);
    }

    fn previous(&self) {
        let _ = self.tx.send(RemoteCommand::Prev);
    }

    fn pause(&self) {
        let _ = self.tx.send(RemoteCommand::Pause);
    }

    fn play_pause(&self) {
        let _ = self.tx.send(RemoteCommand::TogglePlayPause);
    }

    fn stop(&self) {
        let _ = self.tx.send(RemoteCommand::Stop);
    }

    fn play(&self) {
        let _ = self.tx.send(RemoteCommand::Play);
    }

    fn seek(&self, offset: i64) {
        let _ = self.tx.send(RemoteCommand::SeekBy {
            delta_ms: offset / 1000,
        });
    }

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        let view = read_view(&self.view);
        let Some(track) = view.track else {
            return;
        };
        let expected = format!("{TRACK_PATH_PREFIX}{}", track.id);
        if track_id.as_str() != expected.as_str() {
            return; // stale track id: MPRIS says ignore it
        }
        let _ = self.tx.send(RemoteCommand::SeekTo {
            track_id: track.id,
            position_ms: position.max(0) as u64 / 1000,
        });
    }

    fn open_uri(&self, _uri: String) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "OpenUri is not supported by Ampwire".into(),
        ))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn playback_status(&self) -> String {
        let view = read_view(&self.view);
        if !view.running {
            return "Stopped".into();
        }
        match view.state.as_str() {
            "playing" => "Playing".into(),
            "paused" => "Paused".into(),
            _ => "Stopped".into(),
        }
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn loop_status(&self) -> String {
        loop_status_value(&read_view(&self.view)).into()
    }

    #[zbus(property)]
    fn set_loop_status(&self, value: String) -> zbus::fdo::Result<()> {
        let wanted = value != "None";
        if wanted != read_view(&self.view).repeat {
            let _ = self.tx.send(RemoteCommand::ToggleRepeat);
        }
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, _value: f64) -> zbus::fdo::Result<()> {
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn shuffle(&self) -> bool {
        read_view(&self.view).shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&self, value: bool) -> zbus::fdo::Result<()> {
        if value != read_view(&self.view).shuffle {
            let _ = self.tx.send(RemoteCommand::ToggleShuffle);
        }
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        metadata_map(&read_view(&self.view))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn volume(&self) -> f64 {
        read_view(&self.view).volume as f64 / 100.0
    }

    #[zbus(property)]
    fn set_volume(&self, value: f64) -> zbus::fdo::Result<()> {
        let volume = (value.clamp(0.0, 1.0) * 100.0).round() as u8;
        let _ = self.tx.send(RemoteCommand::SetVolume(volume));
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        let view = read_view(&self.view);
        if view.track.is_none() || view.state == "stopped" {
            return 0;
        }
        view.position_ms.saturating_mul(1000).min(i64::MAX as u64) as i64
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_go_next(&self) -> bool {
        can_next(&read_view(&self.view))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_go_previous(&self) -> bool {
        can_previous(&read_view(&self.view))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_play(&self) -> bool {
        read_view(&self.view).playlist.length > 0
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_pause(&self) -> bool {
        let view = read_view(&self.view);
        view.track.is_some() && view.state != "stopped"
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_seek(&self) -> bool {
        let view = read_view(&self.view);
        view.track.is_some() && view.state != "stopped"
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_control(&self) -> bool {
        true
    }
}

fn loop_status_value(view: &View) -> &'static str {
    if view.repeat { "Playlist" } else { "None" }
}

fn can_next(view: &View) -> bool {
    view.track.is_some() && view.playlist.length > 1
}

fn can_previous(view: &View) -> bool {
    can_next(view)
}

fn metadata_map(view: &View) -> HashMap<String, OwnedValue> {
    let Some(track) = view.track.as_ref() else {
        return HashMap::new();
    };
    if view.state == "stopped" {
        return HashMap::new();
    }
    let mut metadata = HashMap::new();
    if let Ok(track_id) = ObjectPath::try_from(format!("{TRACK_PATH_PREFIX}{}", track.id)) {
        metadata.insert("mpris:trackid".into(), OwnedValue::from(track_id));
    }
    if !track.title.is_empty() {
        metadata.insert("xesam:title".into(), owned(track.title.clone()));
    }
    if !track.artist.is_empty() {
        metadata.insert("xesam:artist".into(), owned(vec![track.artist.clone()]));
    }
    if track.duration_ms > 0 {
        metadata.insert(
            "mpris:length".into(),
            owned((track.duration_ms as i64).saturating_mul(1000)),
        );
    }
    metadata
}

/// Wrap a D-Bus-encodable value in the variant form `a{sv}` needs.
fn owned<T: Into<Value<'static>>>(value: T) -> OwnedValue {
    OwnedValue::try_from(value.into()).expect("MPRIS metadata is encodable")
}

#[derive(Clone, PartialEq, Eq)]
struct MprisState {
    state: String,
    track_id: Option<u64>,
    title: String,
    artist: String,
    duration_ms: u64,
    volume: u8,
    shuffle: bool,
    repeat: bool,
    can_next: bool,
    can_previous: bool,
    can_play: bool,
    can_pause: bool,
    can_seek: bool,
}

impl MprisState {
    fn from_view(view: &View) -> Self {
        let track = view.track.as_ref();
        Self {
            state: view.state.clone(),
            track_id: track.map(|track| track.id),
            title: track.map(|track| track.title.clone()).unwrap_or_default(),
            artist: track.map(|track| track.artist.clone()).unwrap_or_default(),
            duration_ms: track.map(|track| track.duration_ms).unwrap_or(0),
            volume: view.volume,
            shuffle: view.shuffle,
            repeat: view.repeat,
            can_next: can_next(view),
            can_previous: can_previous(view),
            can_play: view.playlist.length > 0,
            can_pause: track.is_some() && view.state != "stopped",
            can_seek: track.is_some() && view.state != "stopped",
        }
    }

    fn metadata_changed(&self, other: &Self) -> bool {
        self.track_id != other.track_id
            || self.title != other.title
            || self.artist != other.artist
            || self.duration_ms != other.duration_ms
            // Stopping clears Metadata even though the last track is still
            // current; playing again restores it.
            || (self.state == "stopped") != (other.state == "stopped")
    }
}

fn emitter_loop(conn: Connection, view: SharedView, stop: Arc<AtomicBool>) {
    let mut last: Option<MprisState> = None;
    let mut last_position_ms: Option<u64> = None;

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(POLL);
        if conn.is_closed() {
            break;
        }
        let snapshot = read_view(&view);
        let state = MprisState::from_view(&snapshot);

        if let Some(previous) = &last {
            let mut changed: HashMap<String, OwnedValue> = HashMap::new();
            if previous.state != state.state {
                changed.insert("PlaybackStatus".into(), owned(playback_status(&snapshot)));
            }
            if previous.metadata_changed(&state) {
                changed.insert("Metadata".into(), owned(metadata_map(&snapshot)));
            }
            if previous.shuffle != state.shuffle {
                changed.insert("Shuffle".into(), owned(state.shuffle));
            }
            if previous.repeat != state.repeat {
                changed.insert("LoopStatus".into(), owned(loop_status_value(&snapshot)));
            }
            if previous.volume != state.volume {
                changed.insert("Volume".into(), owned(state.volume as f64 / 100.0));
            }
            if previous.can_next != state.can_next {
                changed.insert("CanGoNext".into(), owned(state.can_next));
            }
            if previous.can_previous != state.can_previous {
                changed.insert("CanGoPrevious".into(), owned(state.can_previous));
            }
            if previous.can_play != state.can_play {
                changed.insert("CanPlay".into(), owned(state.can_play));
            }
            if previous.can_pause != state.can_pause {
                changed.insert("CanPause".into(), owned(state.can_pause));
            }
            if previous.can_seek != state.can_seek {
                changed.insert("CanSeek".into(), owned(state.can_seek));
            }
            if !changed.is_empty() {
                let body = (PLAYER_IFACE, changed, Vec::<String>::new());
                let _ = conn.emit_signal(
                    None::<&str>,
                    OBJECT_PATH,
                    PROPERTIES_IFACE,
                    "PropertiesChanged",
                    &body,
                );
            }

            // Position is polled by clients; Seeked is only for real jumps.
            if let (Some(track_id), Some(previous_position)) = (state.track_id, last_position_ms) {
                let position = snapshot.position_ms;
                let jumped =
                    (position as i64 - previous_position as i64).abs() >= SEEK_THRESHOLD_MS;
                if previous.track_id == Some(track_id) && state.state != "stopped" && jumped {
                    let _ = conn.emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        PLAYER_IFACE,
                        "Seeked",
                        &(position as i64 * 1000),
                    );
                }
                last_position_ms = Some(position);
            } else {
                last_position_ms = None;
            }
        } else {
            last_position_ms = snapshot.position_ms.into();
        }
        last = Some(state);
    }
}

fn playback_status(view: &View) -> String {
    if !view.running {
        return "Stopped".into();
    }
    match view.state.as_str() {
        "playing" => "Playing".into(),
        "paused" => "Paused".into(),
        _ => "Stopped".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::player::mock::MockPlayer;
    use crate::remote::{View, new_view, write_view};

    fn playing_view() -> View {
        View::from_snapshot(&MockPlayer::default().snapshot(), None)
    }

    #[test]
    fn metadata_has_the_mpris_fields() {
        let metadata = metadata_map(&playing_view());
        assert!(metadata.contains_key("mpris:trackid"));
        assert!(metadata.contains_key("xesam:title"));
        assert!(metadata.contains_key("xesam:artist"));
        assert!(metadata.contains_key("mpris:length"));
        let title = String::try_from(metadata["xesam:title"].try_clone().unwrap()).unwrap();
        assert!(!title.is_empty(), "xesam:title is empty");
    }

    #[test]
    fn stopped_view_has_empty_metadata() {
        let mut view = playing_view();
        view.state = "stopped".into();
        assert_eq!(metadata_map(&view).len(), 0);
    }

    #[test]
    fn loop_status_maps_to_mpris_values() {
        let mut view = playing_view();
        view.repeat = false;
        assert_eq!(loop_status_value(&view), "None");
        view.repeat = true;
        assert_eq!(loop_status_value(&view), "Playlist");
    }

    #[test]
    fn can_flags_follow_the_playlist() {
        let view = playing_view();
        assert!(can_next(&view));
        assert!(can_previous(&view));
        let mut single = view;
        single.playlist.length = 1;
        assert!(!can_next(&single));
        assert!(!can_previous(&single));
    }

    #[test]
    #[ignore = "needs a session bus; run inside dbus-run-session"]
    fn dbus_smoke() {
        let (tx, rx) = mpsc::channel();
        let view = new_view();
        write_view(&view, playing_view());
        let Some(handle) = start(view, tx) else {
            eprintln!("skipping: no usable session bus");
            return;
        };
        let conn = Connection::session().expect("client session bus");

        let properties = zbus::blocking::Proxy::new(
            &conn,
            BUS_NAME,
            OBJECT_PATH,
            "org.freedesktop.DBus.Properties",
        )
        .expect("properties proxy");
        let value: OwnedValue = properties
            .call("Get", &("org.mpris.MediaPlayer2.Player", "Metadata"))
            .expect("Metadata Get");
        let metadata: HashMap<String, OwnedValue> =
            HashMap::try_from(value).expect("Metadata is a{sv}");
        let title = String::try_from(metadata["xesam:title"].try_clone().unwrap()).unwrap();
        assert!(!title.is_empty(), "xesam:title is empty");
        assert!(metadata.contains_key("mpris:trackid"));

        let player =
            zbus::blocking::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, PLAYER_IFACE).expect("proxy");
        let _: () = player.call("Next", &()).expect("Next call");
        match rx.recv_timeout(Duration::from_secs(2)).expect("command") {
            RemoteCommand::Next => {}
            other => panic!("expected Next, got {other:?}"),
        }
        drop(handle);
    }
}
