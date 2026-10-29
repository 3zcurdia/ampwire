//! A small, serializable view of the running session, shared by the control
//! socket (`control.rs`) and the MPRIS service (`mpris.rs`).
//!
//! The UI thread owns the player. These services only ever *read* the latest
//! [`View`] (under a mutex) and send [`RemoteCommand`]s back over one channel
//! that `App::tick` drains. Neither is allowed to touch the audio engine.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::player::{FormatInfo, PlayState, PlayerSnapshot, TrackSource};
use crate::smb::LoadInput;

/// Bumped whenever the JSON shape below changes.
pub const VIEW_SCHEMA: u32 = 1;

/// The current track, in the shape a status client or a bar widget wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackView {
    /// Stable per-process id; also the MPRIS `mpris:trackid` suffix.
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub duration_ms: u64,
    /// `local`, `jellyfin` or `smb`.
    pub source: String,
}

/// Where the current track sits in the playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistView {
    /// 0-based; `None` when nothing is current.
    pub index: Option<usize>,
    pub length: usize,
}

/// The decoder's format line plus Ampwire's own DSP verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatView {
    pub codec: String,
    pub bit_depth: Option<u8>,
    pub sample_rate: Option<u32>,
    pub channels: String,
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
    /// True only when *Ampwire's* DSP (volume/balance/swap/EQ) is bypassed.
    /// The OS mixer can still resample; `bit_perfect_scope` says so.
    pub bit_perfect: bool,
    pub bit_perfect_scope: String,
}

impl Default for FormatView {
    fn default() -> Self {
        Self {
            codec: String::new(),
            bit_depth: None,
            sample_rate: None,
            channels: String::new(),
            bitrate_kbps: None,
            lossless: false,
            bit_perfect: false,
            bit_perfect_scope: "app".into(),
        }
    }
}

impl From<&FormatInfo> for FormatView {
    fn from(format: &FormatInfo) -> Self {
        Self {
            codec: format.codec.to_string(),
            bit_depth: format.bit_depth,
            sample_rate: format.sample_rate,
            channels: format.channels.to_string(),
            bitrate_kbps: format.bitrate_kbps,
            lossless: format.lossless,
            bit_perfect: format.bit_perfect,
            bit_perfect_scope: "app".into(),
        }
    }
}

/// Everything a status client or MPRIS needs, without borrowing the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub schema: u32,
    /// The running binary's version.
    pub version: String,
    pub running: bool,
    /// `playing`, `paused` or `stopped`.
    pub state: String,
    pub track: Option<TrackView>,
    pub position_ms: u64,
    pub volume: u8,
    pub balance: i8,
    pub shuffle: bool,
    pub repeat: bool,
    pub playlist: PlaylistView,
    pub format: FormatView,
    /// The last user-facing notice (`buffering…`, `skipped …`, …).
    pub notice: Option<String>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            schema: VIEW_SCHEMA,
            version: env!("CARGO_PKG_VERSION").into(),
            running: false,
            state: "stopped".into(),
            track: None,
            position_ms: 0,
            volume: 100,
            balance: 0,
            shuffle: false,
            repeat: false,
            playlist: PlaylistView {
                index: None,
                length: 0,
            },
            format: FormatView::default(),
            notice: None,
        }
    }
}

impl View {
    /// The payload for `ampwire --status --json` when no session is up.
    pub fn not_running() -> Self {
        Self::default()
    }

    /// Build the view from the UI thread's latest snapshot.
    pub fn from_snapshot(snapshot: &PlayerSnapshot, notice: Option<&str>) -> Self {
        let track = snapshot.current_track().map(|track| TrackView {
            id: track.id,
            title: track.title.clone().unwrap_or_else(|| track.display_name()),
            artist: track.artist.clone().unwrap_or_default(),
            duration_ms: duration_ms(track.duration),
            source: source_label(&track.source).into(),
        });
        Self {
            schema: VIEW_SCHEMA,
            version: env!("CARGO_PKG_VERSION").into(),
            running: true,
            state: state_label(snapshot.state).into(),
            track,
            position_ms: if snapshot.current.is_some() {
                duration_ms(Some(snapshot.position))
            } else {
                0
            },
            volume: snapshot.volume,
            balance: snapshot.balance,
            shuffle: snapshot.shuffle,
            repeat: snapshot.repeat,
            playlist: PlaylistView {
                index: snapshot.current,
                length: snapshot.tracks.len(),
            },
            format: FormatView::from(&snapshot.format),
            notice: notice.map(str::to_string),
        }
    }
}

/// Shared latest view. Lock poisoning is not fatal here: a reader that
/// panicked mid-read must not take the whole player down.
pub type SharedView = Arc<Mutex<View>>;

/// Create the shared slot, initially "not running".
pub fn new_view() -> SharedView {
    Arc::new(Mutex::new(View::not_running()))
}

/// Clone the latest view.
pub fn read_view(view: &SharedView) -> View {
    view.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Replace the latest view.
pub fn write_view(view: &SharedView, next: View) {
    *view
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
}

/// Commands the UI thread applies on its next tick. The services send; only
/// `App` mutates the player.
#[derive(Debug, Clone)]
pub enum RemoteCommand {
    Play,
    Pause,
    TogglePlayPause,
    Stop,
    Next,
    Prev,
    ToggleShuffle,
    ToggleRepeat,
    SetVolume(u8),
    /// Relative seek. MPRIS sends microseconds, the socket sends seconds;
    /// both arrive here in milliseconds.
    SeekBy {
        delta_ms: i64,
    },
    /// Absolute seek, but only for the track named by `track_id` (MPRIS
    /// `SetPosition` must be ignored for a stale track).
    SeekTo {
        track_id: u64,
        position_ms: u64,
    },
    /// Replace the playlist and start it (same as CLI paths / `l`).
    Load(Vec<LoadInput>),
    Quit,
}

pub fn state_label(state: PlayState) -> &'static str {
    match state {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Stopped => "stopped",
    }
}

fn source_label(source: &TrackSource) -> &'static str {
    match source {
        TrackSource::Local(_) => "local",
        TrackSource::Remote { .. } => "jellyfin",
        TrackSource::Smb { .. } => "smb",
    }
}

fn duration_ms(duration: Option<Duration>) -> u64 {
    duration.map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::mock::MockPlayer;

    #[test]
    fn view_mirrors_the_snapshot() {
        let snapshot = MockPlayer::default().snapshot();
        let view = View::from_snapshot(&snapshot, Some("buffering…"));
        assert!(view.running);
        assert_eq!(view.state, "playing");
        assert_eq!(view.playlist.length, snapshot.tracks.len());
        assert_eq!(view.playlist.index, snapshot.current);
        let track = view.track.expect("mock has a current track");
        assert_eq!(track.id, snapshot.current_track().unwrap().id);
        assert_eq!(track.source, "local");
        assert_eq!(view.notice.as_deref(), Some("buffering…"));
        assert_eq!(view.format.bit_perfect_scope, "app");
    }

    #[test]
    fn stopped_view_has_no_track() {
        let mut snapshot = MockPlayer::default().snapshot();
        snapshot.state = PlayState::Stopped;
        snapshot.current = None;
        let view = View::from_snapshot(&snapshot, None);
        assert_eq!(view.state, "stopped");
        assert!(view.track.is_none());
        assert_eq!(view.position_ms, 0);
    }

    #[test]
    fn not_running_json_is_stable() {
        let json = serde_json::to_string(&View::not_running()).unwrap();
        assert!(json.contains("\"running\":false"));
        assert!(json.contains("\"schema\":1"));
        assert!(json.contains("\"state\":\"stopped\""));
    }

    #[test]
    fn view_round_trips_through_json() {
        let view = View::from_snapshot(&MockPlayer::default().snapshot(), None);
        let json = serde_json::to_string(&view).unwrap();
        let back: View = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
    }
}
