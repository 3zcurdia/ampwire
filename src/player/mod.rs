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

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
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
    /// Shared (`Arc<str>`): a snapshot is taken every tick, and copying the
    /// format line must not allocate.
    pub codec: Arc<str>,
    pub bit_depth: Option<u8>,
    pub sample_rate: Option<u32>,
    pub channels: Arc<str>,
    pub bit_perfect: bool,
    /// Lossy only: the tagged (or estimated) bitrate. `None` for lossless.
    pub bitrate_kbps: Option<u32>,
    /// FLAC / ALAC / linear PCM: the source samples reach the device
    /// unchanged when no DSP touches them.
    pub lossless: bool,
}

impl FormatInfo {
    /// Nothing known yet: shown when stopped, or between `Load` and the
    /// engine's `Started` event. An empty codec means "nothing playing": the
    /// UI renders no format line.
    pub fn blank() -> Self {
        Self {
            codec: "".into(),
            bit_depth: None,
            sample_rate: None,
            channels: "".into(),
            bit_perfect: false,
            bitrate_kbps: None,
            lossless: false,
        }
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
        parts.push(self.codec.to_string());
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
            parts.push(self.channels.to_string());
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
        let khz = format!("{:.1}", f64::from(rate) / 1000.0);
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

/// The playlist's tracks, shared with snapshots. Each track sits behind its
/// own `Arc`, so copying the list on write (`Arc::make_mut`) only bumps
/// refcounts, and editing one track copies just that track.
pub type TrackList = Arc<Vec<Arc<Track>>>;

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

/// Album position tags (plan 12): what the playlist groups and orders by
/// before falling back to [`Track::alpha_key`]. All `None` for a file
/// without such tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumPos {
    pub album: Option<String>,
    pub album_artist: Option<String>,
    /// Disc number, `None` when missing or `0`.
    pub disc: Option<u32>,
    /// Track number on the disc, `None` when missing or `0`.
    pub number: Option<u32>,
}

/// What [`Playlist::append_sorted`][crate::player::playlist::Playlist::append_sorted]
/// sorts on (plan 12): album group, disc, track number, then today's
/// alphabetical key. Field order is the comparison order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SortKey {
    group: String,
    disc: u32,
    number: u32,
    alpha: String,
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
    /// Album/disc/track tags that drive the sort (plan 12).
    pub pos: AlbumPos,
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

    /// Today's alphabetical order (plans 02/08/10), used as the tie-break
    /// and as the whole key when a track has no track number.
    pub fn alpha_key(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => format!("{artist} - {title}").to_lowercase(),
            _ => match &self.source {
                TrackSource::Local(path) => path.to_string_lossy().to_lowercase(),
                TrackSource::Remote { .. } => self.title.clone().unwrap_or_default().to_lowercase(),
                TrackSource::Smb { url } => url.canonical().to_lowercase(),
            },
        }
    }

    /// Sort order (plan 12): a track with a track number sorts inside its
    /// album group by disc (missing = 1) then number; a track without one
    /// sorts exactly as before ([`alpha_key`][Self::alpha_key]).
    pub fn sort_key(&self) -> SortKey {
        let alpha = self.alpha_key();
        match self.pos.number {
            Some(number) => SortKey {
                group: self.album_group().unwrap_or_else(|| self.folder_key()),
                disc: self.pos.disc.unwrap_or(1),
                number,
                alpha,
            },
            None => SortKey {
                group: alpha.clone(),
                disc: 0,
                number: 0,
                alpha,
            },
        }
    }

    /// `"<album artist> - <album>"` lowercased (album artist falls back to
    /// the track artist); `None` without an album tag.
    fn album_group(&self) -> Option<String> {
        let album = self.pos.album.as_deref()?;
        let artist = self.pos.album_artist.as_deref().or(self.artist.as_deref());
        let group = match artist {
            Some(artist) => format!("{artist} - {album}"),
            None => album.to_string(),
        };
        Some(group.to_lowercase())
    }

    /// The folder holding the track, lowercased, ending in `/` (so a
    /// folder's numbered tracks sort right before its un-numbered files,
    /// whose path key starts with the same text). A Jellyfin row has no
    /// folder and falls back to its alpha key.
    fn folder_key(&self) -> String {
        match &self.source {
            TrackSource::Local(path) => match path.parent() {
                Some(dir) => format!("{}/", dir.to_string_lossy().to_lowercase()),
                None => self.alpha_key(),
            },
            TrackSource::Smb { url } => {
                let canonical = url.canonical().to_lowercase();
                match canonical.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/"),
                    None => canonical,
                }
            }
            TrackSource::Remote { .. } => self.alpha_key(),
        }
    }

    /// A Jellyfin item specifically: the rules that are Jellyfin's alone —
    /// the `j` reload rule ([`RemoveRemote`][PlayerCommand::RemoveRemote]),
    /// the Jellyfin download window and `stream_pure`'s EQ bypass — key off
    /// this, never off a "remote" test (SMB tracks must not
    /// be dropped, downloaded or EQ-bypassed by a Jellyfin action).
    pub fn is_jellyfin(&self) -> bool {
        matches!(self.source, TrackSource::Remote { .. })
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
    pub tracks: TrackList,
    pub shuffle: bool,
    pub repeat: bool,
    /// 0–=100.
    pub volume: u8,
    /// -100..=100.
    pub balance: i8,
    pub format: FormatInfo,
    /// Spectrum magnitudes, 0.0–1.0, low to high frequency.
    pub spectrum: Bars,
    /// Spectrum peak caps, same length as `spectrum`.
    pub peaks: Bars,
}

/// Spectrum values held inline (at most [`crate::audio::analysis::BANDS`]),
/// so a snapshot copies them without a heap allocation. Derefs to the bars
/// as a slice: `len()`, indexing and iteration all work as on a `Vec`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bars {
    len: usize,
    values: [f32; crate::audio::analysis::BANDS],
}

impl Bars {
    /// A copy of `bars`. Panics if there are more than `BANDS` of them: the
    /// analyzer and the mock both stay within that.
    pub fn from_slice(bars: &[f32]) -> Self {
        let mut values = [0.0; crate::audio::analysis::BANDS];
        values[..bars.len()].copy_from_slice(bars);
        Self {
            len: bars.len(),
            values,
        }
    }
}

impl Deref for Bars {
    type Target = [f32];

    fn deref(&self) -> &[f32] {
        &self.values[..self.len]
    }
}

impl PlayerSnapshot {
    pub fn current_track(&self) -> Option<&Track> {
        self.current
            .and_then(|i| self.tracks.get(i))
            .map(Arc::as_ref)
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
    /// Absolute seek (MPRIS `SetPosition`, the control socket): reload the
    /// current track at `target`, keeping the paused state.
    SeekTo(Duration),
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
    /// `AlbumArtist, Album, ParentIndexNumber, IndexNumber, SortName`
    /// order instead of being re-sorted client-side.
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
            pos: crate::player::AlbumPos::default(),
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
    fn alpha_key_is_artist_title_or_path_lowercase() {
        assert_eq!(
            local("/M/A.flac", Some("Aphex Twin"), Some("Xtal")).alpha_key(),
            "aphex twin - xtal"
        );
        assert_eq!(
            local("/M/01 Intro.flac", None, None).alpha_key(),
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
            pos: crate::player::AlbumPos::default(),
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
            pos: crate::player::AlbumPos::default(),
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
            smb("Album/01 Intro.flac", None, None).alpha_key(),
            "smb://nas/music/album/01 intro.flac"
        );
        assert_eq!(smb("", None, None).alpha_key(), "smb://nas/music");
    }

    /// Plan 10, step 2: a non-local source covers both Jellyfin and SMB (the
    /// remote-only UI rules), while the three Jellyfin-specific rules stay
    /// Jellyfin-only.
    #[test]
    fn smb_tracks_are_remote_but_not_jellyfin() {
        let share = smb("a.flac", None, None);
        assert!(
            !matches!(share.source, crate::player::TrackSource::Local(_)),
            "SMB is not a local file"
        );
        assert!(!share.is_jellyfin(), "…but it is not a Jellyfin item");

        let server = jellyfin("abc");
        assert!(
            !matches!(server.source, crate::player::TrackSource::Local(_)) && server.is_jellyfin()
        );

        let disk = local("/m/a.flac", None, None);
        assert!(matches!(disk.source, crate::player::TrackSource::Local(_)) && !disk.is_jellyfin());
    }

    // --- album order (plan 12) ---

    /// A local track with album-position tags (plan 12).
    fn numbered(path: &str, album: Option<&str>, disc: Option<u32>, number: Option<u32>) -> Track {
        let mut t = local(path, Some("Artist"), Some(path));
        t.pos = AlbumPos {
            album: album.map(str::to_string),
            album_artist: None,
            disc,
            number,
        };
        t
    }

    /// Plan 12: within one album, disc 1 (tracks 1, 2, 9) sorts before
    /// disc 2 — the disc boundary is compared before the track number.
    #[test]
    fn disc_then_number_within_an_album() {
        let d1t1 = numbered("/m/a/01.flac", Some("Album"), Some(1), Some(1));
        let d1t2 = numbered("/m/a/02.flac", Some("Album"), Some(1), Some(2));
        let d1t9 = numbered("/m/a/09.flac", Some("Album"), Some(1), Some(9));
        let d2t1 = numbered("/m/a/2-01.flac", Some("Album"), Some(2), Some(1));

        assert!(d1t1.sort_key() < d1t2.sort_key(), "track 1 before track 2");
        assert!(d1t2.sort_key() < d1t9.sort_key(), "track 2 before track 9");
        assert!(
            d1t9.sort_key() < d2t1.sort_key(),
            "disc 1 track 9 before disc 2 track 1"
        );
    }

    /// Plan 12: "no disc tag" and "disc 1" merge — a missing disc counts
    /// as disc 1, so (None, 2) sits between (1, 1) and (1, 3).
    #[test]
    fn missing_disc_counts_as_disc_one() {
        let one = numbered("/m/a/01.flac", Some("Album"), Some(1), Some(1));
        let two = numbered("/m/a/02.flac", Some("Album"), None, Some(2));
        let three = numbered("/m/a/03.flac", Some("Album"), Some(1), Some(3));

        assert!(one.sort_key() < two.sort_key(), "(1,1) < (None,2)");
        assert!(two.sort_key() < three.sort_key(), "(None,2) < (1,3)");
    }

    /// Plan 12: the track number beats the title alphabet — track 1
    /// "Zebra" sorts before track 2 "Apple".
    #[test]
    fn track_number_beats_title_alphabet() {
        let mut zebra = numbered("/m/a/01.flac", Some("Album"), Some(1), Some(1));
        zebra.title = Some("Zebra".into());
        let mut apple = numbered("/m/a/02.flac", Some("Album"), Some(1), Some(2));
        apple.title = Some("Apple".into());

        assert!(
            zebra.sort_key() < apple.sort_key(),
            "the number wins over the title"
        );
    }

    /// Plan 12: without a track number the whole key is the alpha key
    /// (group = alpha, disc/number 0) — even with an album tag — so
    /// un-numbered tracks order exactly like `alpha_key()`.
    #[test]
    fn no_number_falls_back_to_alpha_key() {
        let a = local("/m/a.flac", Some("Aphex Twin"), Some("Xtal"));
        let mut b = local("/m/b.flac", Some("Boards of Canada"), Some("Roygbiv"));
        b.pos.album = Some("Greatest".into()); // album tag, but no number

        assert_eq!(
            a.sort_key(),
            SortKey {
                group: a.alpha_key(),
                disc: 0,
                number: 0,
                alpha: a.alpha_key(),
            }
        );
        assert_eq!(
            a.sort_key().cmp(&b.sort_key()),
            a.alpha_key().cmp(&b.alpha_key()),
            "un-numbered tracks order exactly like alpha_key()"
        );
    }

    /// Plan 12: two "Greatest Hits" albums by different album artists do
    /// not interleave — all of A's tracks, then all of B's.
    #[test]
    fn album_artist_separates_same_named_albums() {
        let mut a1 = numbered("/m/a/01.flac", Some("Greatest Hits"), None, Some(1));
        a1.pos.album_artist = Some("Artist A".into());
        let mut a2 = numbered("/m/a/02.flac", Some("Greatest Hits"), None, Some(2));
        a2.pos.album_artist = Some("Artist A".into());
        let mut b1 = numbered("/m/b/01.flac", Some("Greatest Hits"), None, Some(1));
        b1.pos.album_artist = Some("Artist B".into());
        let mut b2 = numbered("/m/b/02.flac", Some("Greatest Hits"), None, Some(2));
        b2.pos.album_artist = Some("Artist B".into());

        let mut tracks = [b2, a1, b1, a2];
        tracks.sort_by_key(super::Track::sort_key);
        assert_eq!(
            tracks
                .iter()
                .map(|t| t.title.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            [
                "/m/a/01.flac".to_string(),
                "/m/a/02.flac".to_string(),
                "/m/b/01.flac".to_string(),
                "/m/b/02.flac".to_string(),
            ],
            "each album artist's tracks stay together"
        );
    }

    /// Plan 12: numbered tracks without an album tag group by folder —
    /// all of `/m/a` (by number), then the folder's untagged file (its
    /// path key starts with the same text), then all of `/m/b`.
    #[test]
    fn folder_groups_numbered_tracks_without_album_tag() {
        let a1 = numbered("/m/a/01.flac", None, None, Some(1));
        let a2 = numbered("/m/a/02.flac", None, None, Some(2));
        let b1 = numbered("/m/b/01.flac", None, None, Some(1));
        // Untagged, so its alpha key is the path.
        let x = local("/m/a/x.flac", None, None);

        let mut tracks = [b1, x, a2, a1];
        tracks.sort_by_key(super::Track::sort_key);
        let paths: Vec<String> = tracks
            .iter()
            .map(|t| match &t.source {
                TrackSource::Local(path) => path.to_string_lossy().into_owned(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                "/m/a/01.flac".to_string(),
                "/m/a/02.flac".to_string(),
                "/m/a/x.flac".to_string(),
                "/m/b/01.flac".to_string(),
            ]
        );
    }

    /// Plan 12: an SMB row's folder group drops the file name —
    /// `…/Album/03.flac` groups as `smb://nas/music/album/`.
    #[test]
    fn smb_folder_key_drops_the_file_name() {
        let mut t = smb("Album/03.flac", Some("Artist"), Some("Song"));
        t.pos.number = Some(3); // numbered, but no album tag → folder group

        assert_eq!(t.sort_key().group, "smb://nas/music/album/");
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
        assert_eq!(FormatInfo::blank().describe(), "", "no format line");
        assert_eq!(FormatInfo::blank().format_parts().len(), 0);
    }
}
