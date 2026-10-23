//! Player view model and commands.
//!
//! `ui` only ever reads a [`PlayerSnapshot`]. Changes go the other way, as a
//! [`PlayerCommand`]. Two backends implement that contract: [`mock::MockPlayer`]
//! (demo / `--dump` / tests) and [`real::RealPlayer`] (a real session, whose
//! audio thread is [`crate::audio::Engine`]) — the UI cannot tell them apart.

pub mod mock;
pub mod order;
pub mod playlist;
pub mod real;
pub mod rng;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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

impl FormatInfo {
    /// Nothing known yet: shown when stopped, or between `Load` and the
    /// engine's `Started` event.
    pub fn blank() -> Self {
        Self {
            codec: String::new(),
            bit_depth: None,
            sample_rate: None,
            channels: String::new(),
            bit_perfect: false,
        }
    }
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

/// Stable per-process track id, from a static counter.
pub type TrackId = u64;

/// Where a track's audio comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    Local(PathBuf),
    // plan 08 adds `Remote { .. }`
}

/// One playlist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId, // unique per process, from a static AtomicU64
    pub source: TrackSource,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub duration: Option<Duration>,
}

impl Track {
    /// The next unique track id (relaxed: uniqueness is all that matters).
    pub fn next_id() -> TrackId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// Row text: `artist - title` when both tags exist, otherwise the file
    /// name without extension (`?` when even that is missing).
    pub fn display_name(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} - {title}"),
            _ => match &self.source {
                TrackSource::Local(path) => path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("?")
                    .to_string(),
            },
        }
    }

    /// Sort order: `artist - title` (lowercased) when both tags exist,
    /// otherwise the full path as a lowercased string.
    pub fn sort_key(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} - {title}").to_lowercase(),
            _ => match &self.source {
                TrackSource::Local(path) => path.to_string_lossy().to_lowercase(),
            },
        }
    }
}

/// Everything the UI needs to render the player, in one immutable snapshot.
#[derive(Debug, Clone, Default)]
pub struct PlayerSnapshot {
    pub state: PlayState,
    /// Index into `tracks` of the current track, if any.
    pub current: Option<usize>,
    pub position: Duration,
    /// Arc'd so snapshots are cheap (30 fps), not a full clone.
    pub tracks: std::sync::Arc<Vec<Track>>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Append, sorted among themselves.
    AddTracks(Vec<Track>),
    /// Replace the playlist, then behave like `PlayTrack(0)` if non-empty.
    ReplaceTracks(Vec<Track>),
    RemoveTrack(usize),
    ClearPlaylist,
    /// Move a track; `up = true` moves it up.
    MoveTrack {
        from: usize,
        up: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, artist: Option<&str>, title: Option<&str>) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Local(PathBuf::from(path)),
            artist: artist.map(str::to_string),
            title: title.map(str::to_string),
            duration: None,
        }
    }

    #[test]
    fn display_name_prefers_both_tags_then_stem() {
        assert_eq!(
            local("/m/01 intro.flac", Some("Aphex"), Some("Xtal")).display_name(),
            "Aphex - Xtal"
        );
        // Title only (or artist only) falls back to the file name stem.
        assert_eq!(
            local("/m/01 intro.flac", None, Some("Xtal")).display_name(),
            "01 intro"
        );
        assert_eq!(
            local("/m/01 intro.flac", Some("Aphex"), None).display_name(),
            "01 intro"
        );
        // No stem at all → `?`.
        assert_eq!(local("/", None, None).display_name(), "?");
    }

    #[test]
    fn sort_key_is_artist_title_or_path_lowercase() {
        assert_eq!(
            local("/M/A.flac", Some("Aphex Twin"), Some("Xtal")).sort_key(),
            "aphex twin - xtal"
        );
        assert_eq!(
            local("/M/01 Intro.flac", None, None).sort_key(),
            "/m/01 intro.flac"
        );
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(Track::next_id(), Track::next_id());
    }
}
