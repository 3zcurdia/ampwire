//! Player view model and commands.
//!
//! `ui` only ever reads a [`PlayerSnapshot`]. Changes go the other way, as a
//! [`PlayerCommand`]. Two backends implement that contract: [`mock::MockPlayer`]
//! (demo / `--dump` / tests) and [`real::RealPlayer`] (a real session, whose
//! audio thread is [`crate::audio::Engine`]) — the UI cannot tell them apart.

pub mod eq_presets;
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
    /// Lossy only: the tagged (or estimated) bitrate. `None` for lossless.
    pub bitrate_kbps: Option<u32>,
    /// FLAC / ALAC / linear PCM: the source samples reach the device
    /// unchanged when no DSP touches them.
    pub lossless: bool,
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
            bitrate_kbps: None,
            lossless: false,
        }
    }

    /// "Nothing playing": codec empty, so the UI renders no format line.
    pub fn empty() -> Self {
        Self::blank()
    }

    /// The parts of the format line, widest first: codec, depth/rate
    /// (lossless) or bitrate/rate (lossy), channels — each omitted when
    /// unknown (and everything when nothing plays). The UI joins them and
    /// may drop the trailing ones on a narrow panel.
    pub fn format_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.codec.is_empty() {
            return parts; // nothing playing: no format line
        }
        parts.push(self.codec.clone());
        let rate = self.sample_rate.map(format_khz);
        if self.lossless {
            match (self.bit_depth, &rate) {
                (Some(depth), Some(rate)) => parts.push(format!("{depth}/{rate}")),
                (Some(depth), None) => parts.push(format!("{depth}-bit")),
                (None, Some(rate)) => parts.push(format!("{rate}kHz")),
                (None, None) => {}
            }
        } else {
            if let Some(kbps) = self.bitrate_kbps {
                parts.push(format!("{kbps}kbps"));
            }
            if let Some(rate) = &rate {
                parts.push(format!("{rate}kHz"));
            }
        }
        if !self.channels.is_empty() {
            parts.push(self.channels.clone());
        }
        parts
    }

    /// The format-line text, e.g. `FLAC  24/96  stereo` (lossless) or
    /// `MP3  320kbps  44.1kHz  stereo` (lossy). Empty when nothing plays.
    /// The `bit-perfect` marker is *not* part of it — the UI keeps that
    /// exactly where it was.
    pub fn describe(&self) -> String {
        self.format_parts().join("  ")
    }
}

/// Sample rate in kHz, keeping the fraction only when there is one:
/// 96000 → `96`, 44100 → `44.1`, 88200 → `88.2`.
pub fn format_khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{}", rate / 1000)
    } else {
        let khz = format!("{:.1}", rate as f64 / 1000.0);
        khz.trim_end_matches(".0").to_string()
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
            bitrate_kbps: None,
            lossless: true,
        }
    }
}

/// Stable per-process track id, from a static counter.
pub type TrackId = u64;

/// Where a track's audio comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    Local(PathBuf),
    /// A Jellyfin item (plan 08): the server's `Id`, plus the container used
    /// as the file-extension hint (decoder probe / cache file name). The
    /// credentials never live here — the [`Client`][crate::jellyfin::Client]
    /// holds them.
    Remote {
        item_id: crate::jellyfin::ItemId,
        container: Option<String>,
    },
    /// One `smb://` URL (plan 10): canonical server/share/path, no
    /// credentials — those live in `[smb]` config and reach the client
    /// setup directly.
    Smb {
        url: crate::smb::SmbUrl,
    },
}

/// The file-name stem of `path`, `?` when there is none (root, a trailing
/// slash, a name without a stem). Shared by the local and the `smb://`
/// branch of [`Track::display_name`], which read a path the same way.
fn path_stem(path: &std::path::Path) -> &str {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("?")
}

/// One playlist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId, // unique per process, from a static AtomicU64
    pub source: TrackSource,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub duration: Option<Duration>,
    /// Lossy only: tagged (or estimated) bitrate in kbps. `None` for
    /// lossless sources and unknown files.
    pub bitrate_kbps: Option<u32>,
}

impl Track {
    /// The next unique track id (relaxed: uniqueness is all that matters).
    pub fn next_id() -> TrackId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// Row text: `artist - title` when both tags exist, otherwise the file
    /// name without extension (`?` when even that is missing). A remote row
    /// never shows a path: it falls back to the server's `Name`.
    pub fn display_name(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} - {title}"),
            _ => match &self.source {
                TrackSource::Local(path) => path_stem(path).to_string(),
                TrackSource::Remote { .. } => self.title.clone().unwrap_or_else(|| "?".into()),
                // Same rule as a local file: the last segment of the path
                // (the share root has no file name → `?`).
                TrackSource::Smb { url } => path_stem(std::path::Path::new(&url.path)).to_string(),
            },
        }
    }

    /// Sort order: `artist - title` (lowercased) when both tags exist,
    /// otherwise the full path as a lowercased string. A remote row has no
    /// path, so it sorts on the title (the server's `Name`); an SMB row
    /// sorts on its canonical URL.
    pub fn sort_key(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} - {title}").to_lowercase(),
            _ => match &self.source {
                TrackSource::Local(path) => path.to_string_lossy().to_lowercase(),
                TrackSource::Remote { .. } => self.title.clone().unwrap_or_default().to_lowercase(),
                TrackSource::Smb { url } => url.canonical().to_lowercase(),
            },
        }
    }

    /// Not a local file — Jellyfin *or* SMB (plan 10). Drives the
    /// remote-only cache windows (one per source) and the "no path in the
    /// row" rules.
    pub fn is_remote(&self) -> bool {
        matches!(
            self.source,
            TrackSource::Remote { .. } | TrackSource::Smb { .. }
        )
    }

    /// A Jellyfin item specifically: the rules that are Jellyfin's alone —
    /// the `j` reload rule ([`RemoveRemote`][PlayerCommand::RemoveRemote]),
    /// the Jellyfin download window and `stream_pure`'s EQ bypass — key off
    /// this, never off [`is_remote`][Self::is_remote] (SMB tracks must not
    /// be dropped, downloaded or EQ-bypassed by a Jellyfin action).
    pub fn is_jellyfin(&self) -> bool {
        matches!(self.source, TrackSource::Remote { .. })
    }

    /// An SMB row specifically — what the SMB rolling cache window (plan
    /// 10, step 5) and anything else SMB-only selects. Expressed through
    /// the other two so the relation stays visible: remote, and not the
    /// Jellyfin half of them.
    pub fn is_smb(&self) -> bool {
        self.is_remote() && !self.is_jellyfin()
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
    /// Absolute balance, −100..=100.
    SetBalance(i8),
    /// Route L/R to the opposite ears (`[audio] swap_channels`) — for
    /// headsets/DACs with reversed channel wiring.
    SetSwapChannels(bool),
    /// The equalizer (plan 06): on/off + preamp + 10 band gains in whole dB
    /// (`gains[0]` = preamp). This payload is why `PlayerCommand` lost
    /// `Copy` back in plan 02.
    SetEq {
        enabled: bool,
        gains: [i8; 11],
    },
    ToggleShuffle,
    ToggleRepeat,
    /// Append, sorted among themselves.
    AddTracks(Vec<Track>),
    /// Append without sorting (plan 08): remote tracks keep the server's
    /// `Artist, SortName` order instead of being re-sorted client-side.
    AppendInOrder(Vec<Track>),
    /// Remove every Jellyfin track (the `j` reload rule: drop the old
    /// remote batch before the new one arrives). Stops playback when the
    /// current track was one of them — same as [`RemoveTrack`].
    RemoveRemote,
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
            bitrate_kbps: None,
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

    // --- plan 10, step 2: `TrackSource::Smb` ---

    /// An SMB row, built straight from its URL (no parser involved).
    fn smb(path: &str, artist: Option<&str>, title: Option<&str>) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Smb {
                url: crate::smb::SmbUrl {
                    server: "NAS".into(),
                    share: "Music".into(),
                    path: path.into(),
                },
            },
            artist: artist.map(str::to_string),
            title: title.map(str::to_string),
            duration: None,
            bitrate_kbps: None,
        }
    }

    /// A Jellyfin row (the other remote kind), for the predicate contrast.
    fn jellyfin(id: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Remote {
                item_id: id.into(),
                container: None,
            },
            artist: None,
            title: Some(format!("title-{id}")),
            duration: None,
            bitrate_kbps: None,
        }
    }

    /// Plan 10, step 2: an SMB row reads like a local one — tags first,
    /// then the file-name stem of the URL path (`?` at the share root) —
    /// and sorts on the canonical URL, lowercased.
    #[test]
    fn smb_rows_display_and_sort_like_local_files() {
        // Tags first, exactly like any other row.
        assert_eq!(
            smb("Album/01.flac", Some("Aphex"), Some("Xtal")).display_name(),
            "Aphex - Xtal"
        );
        // …then the stem of the path (title-only and artist-only too).
        assert_eq!(
            smb("Album/01 intro.flac", None, Some("Xtal")).display_name(),
            "01 intro"
        );
        assert_eq!(
            smb("Album/01 intro.flac", Some("Aphex"), None).display_name(),
            "01 intro"
        );
        // The share root is a directory, not a file: `?`.
        assert_eq!(smb("", None, None).display_name(), "?");

        // Sorting: canonical URL, lowercased (the path keeps its case
        // until the sort folds it, and the server is folded first).
        assert_eq!(
            smb("Album/01 Intro.flac", None, None).sort_key(),
            "smb://nas/music/album/01 intro.flac"
        );
        assert_eq!(smb("", None, None).sort_key(), "smb://nas/music");
    }

    /// Plan 10, step 2: `is_remote()` covers both non-local sources (the
    /// remote-only UI rules), while the three Jellyfin-specific rules stay
    /// Jellyfin-only.
    #[test]
    fn smb_tracks_are_remote_but_not_jellyfin() {
        let share = smb("a.flac", None, None);
        assert!(share.is_remote(), "SMB is not a local file");
        assert!(!share.is_jellyfin(), "…but it is not a Jellyfin item");

        let server = jellyfin("abc");
        assert!(server.is_remote() && server.is_jellyfin());

        let disk = local("/m/a.flac", None, None);
        assert!(!disk.is_remote() && !disk.is_jellyfin());
    }

    // --- format line (plan 05, step 3 + 7) ---

    #[test]
    fn khz_keeps_fractions() {
        assert_eq!(format_khz(44_100), "44.1");
        assert_eq!(format_khz(88_200), "88.2");
        assert_eq!(format_khz(48_000), "48");
        assert_eq!(format_khz(96_000), "96");
        assert_eq!(format_khz(352_800), "352.8");
    }

    /// The lossless / lossy / missing-field forms of `describe()`.
    #[test]
    fn describe_covers_every_line_shape() {
        // Lossless, everything known (the mock default; goldens pin this).
        assert_eq!(FormatInfo::default().describe(), "FLAC  24/96  stereo");
        // Lossless, depth unknown → rate only.
        let flac = FormatInfo {
            bit_depth: None,
            ..FormatInfo::default()
        };
        assert_eq!(flac.describe(), "FLAC  96kHz  stereo");
        // Lossless, rate unknown → depth only.
        let flac = FormatInfo {
            sample_rate: None,
            ..FormatInfo::default()
        };
        assert_eq!(flac.describe(), "FLAC  24-bit  stereo");
        // Lossless, neither known → codec + channels.
        let flac = FormatInfo {
            bit_depth: None,
            sample_rate: None,
            ..FormatInfo::default()
        };
        assert_eq!(flac.describe(), "FLAC  stereo");
        // Lossy, everything known.
        let full = FormatInfo {
            codec: "MP3".into(),
            bit_depth: None,
            sample_rate: Some(44_100),
            channels: "stereo".into(),
            bit_perfect: false,
            bitrate_kbps: Some(320),
            lossless: false,
        };
        assert_eq!(full.describe(), "MP3  320kbps  44.1kHz  stereo");
        // Lossy, bitrate unknown → the kbps part is omitted.
        let no_kbps = FormatInfo {
            bitrate_kbps: None,
            ..full.clone()
        };
        assert_eq!(no_kbps.describe(), "MP3  44.1kHz  stereo");
        // Lossy, rate unknown → the rate part is omitted.
        let no_rate = FormatInfo {
            sample_rate: None,
            ..full
        };
        assert_eq!(no_rate.describe(), "MP3  320kbps  stereo");
        // Mono and "nothing playing".
        let mono = FormatInfo {
            channels: "mono".into(),
            ..FormatInfo::default()
        };
        assert_eq!(mono.describe(), "FLAC  24/96  mono");
        assert_eq!(FormatInfo::empty().describe(), "", "no format line");
        assert!(FormatInfo::empty().format_parts().is_empty());
    }
}
