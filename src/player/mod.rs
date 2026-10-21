//! Player view model and commands.
//!
//! `ui` only ever reads a [`PlayerSnapshot`]. Changes go the other way, as a
//! [`PlayerCommand`]. This phase feeds commands to [`mock::MockPlayer`];
//! later a channel to the audio thread replaces it without touching render
//! code.

pub mod mock;

use std::time::Duration;

/// Transport state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Format of the current track — the hi-fi line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatInfo {
    pub codec: String,
    pub bit_depth: Option<u8>,
    pub sample_rate: Option<u32>,
    pub channels: String,
    pub bit_perfect: bool,
}

impl Default for FormatInfo {
    fn default() -> Self {
        Self {
            codec: "FLAC".into(),
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            channels: "stereo".into(),
            bit_perfect: true,
        }
    }
}

/// One playlist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub duration: Duration,
}

/// Everything the UI needs to render the player, in one immutable snapshot.
#[derive(Debug, Clone, Default)]
pub struct PlayerSnapshot {
    pub state: PlayState,
    /// Index into `tracks` of the current track, if any.
    pub current: Option<usize>,
    pub position: Duration,
    pub tracks: Vec<Track>,
    pub shuffle: bool,
    pub repeat: bool,
    /// 0–=100.
    pub volume: u8,
    /// -100..=100.
    pub balance: i8,
    pub format: FormatInfo,
    /// Spectrum magnitudes, 0.0–1.0, low to high frequency.
    pub spectrum: Vec<f32>,
    /// Spectrum peak caps, same length as `spectrum`.
    pub peaks: Vec<f32>,
}

impl PlayerSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.tracks.get(i))
    }
}

/// Commands sent from the UI toward the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerCommand {
    Play,
    Pause,
    Stop,
    TogglePlayPause,
    Next,
    Prev,
    PlayTrack(usize),
    SeekBy(i64),
    /// Absolute volume, 0–=100.
    SetVolume(u8),
    ToggleShuffle,
    ToggleRepeat,
}
