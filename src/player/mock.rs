//! Mock player: fake playlist, ticking clock, animated spectrum.
//!
//! Stands in for [`RealPlayer`](super::real::RealPlayer) in `--demo`,
//! `--dump` and the tests — same clock/spectrum contract, faked data (the
//! real one counts audio frames and runs an FFT). The RNG is seeded so
//! a given seed always yields the same spectrum; [`MockPlayer::deterministic`]
//! freezes that seed's spectrum so `--dump` and the golden tests reproduce
//! exactly.

use std::path::PathBuf;
use std::time::Duration;

use super::playlist::Playlist;
use super::rng::Rng;
use super::{FormatInfo, PlayState, PlayerCommand, PlayerSnapshot, Track, TrackSource};

const SPECTRUM_BINS: usize = 24;

/// Seed for the animated (non-deterministic) player.
const ANIMATED_SEED: u64 = 42;
/// Seed for the frozen `--dump` / golden-test spectrum.
const DETERMINISTIC_SEED: u64 = 0x5EED_C0DE;
/// Assumed length when a track's duration is unknown (end-of-track check).
const UNKNOWN_DURATION: Duration = Duration::from_secs(180);

pub struct MockPlayer {
    state: PlayState,
    playlist: Playlist,
    position: Duration,
    shuffle: bool,
    repeat: bool,
    volume: u8,
    balance: i8,
    format: FormatInfo,
    spectrum: Vec<f32>,
    /// Peaks slowly fall back toward the bar height, btop-style.
    peaks: Vec<f32>,
    rng: Rng,
    /// When true the spectrum and peaks never move (only the clock does), so
    /// `--dump --frame N` and the golden tests are byte-for-byte reproducible.
    frozen: bool,
}

/// Build a plausible-looking spectrum from a seeded RNG: a rolling hill that
/// spans the full 0–1 range (so bars can drop to zero), low bins louder than
/// high, with per-bin jitter. Same seed → same shape.
fn seeded_spectrum(seed: u64) -> (Vec<f32>, Vec<f32>, Rng) {
    let mut rng = Rng::new(seed);
    let mut spectrum = vec![0.0f32; SPECTRUM_BINS];
    for (i, bin) in spectrum.iter_mut().enumerate() {
        let t = i as f32 / (SPECTRUM_BINS - 1) as f32;
        // A rolling hill: peaks near the low end, dips that reach zero.
        let hill = (0.55 + 0.45 * (t * 6.0).sin()) * (1.0 - 0.55 * t);
        let jitter = 0.35 * rng.next_f32();
        *bin = (hill * (0.45 + 0.55 * rng.next_f32()) + jitter - 0.18).clamp(0.0, 1.0);
    }
    // Peak caps track the bars exactly in the frozen state so a zero bar has
    // a zero peak (the animated path re-derives peaks each tick anyway).
    let peaks = spectrum.clone();
    (spectrum, peaks, rng)
}

/// 24 fake tracks of imaginary downtempo. The rendered text must stay
/// identical to the raw `"artist - title"` strings, so golden frames don't
/// change: split on `" - "` into tags.
fn fake_tracks() -> Vec<Track> {
    let raw = [
        ("Aphex Twin - Xtal", 291),
        ("Boards of Canada - Roygbiv", 151),
        ("Daft Punk - Veridis Quo", 345),
        ("Massive Attack - Teardrop", 329),
        ("Portishead - Roads", 305),
        ("Burial - Archangel", 238),
        ("Air - La Femme d'Argent", 431),
        ("Moderat - A New Error", 346),
        ("Bonobo - Kerala", 245),
        ("Tycho - Awake", 284),
        ("Röyksopp - Eple", 219),
        ("Kiasmos - Blurred EP", 367),
        ("Jon Hopkins - Open Eye Signal", 468),
        ("Four Tet - Two Thousand", 312),
        ("Floating Points - Ratio", 390),
        ("Caribou - Sun", 297),
        ("Boards of Canada - Dayvan Cowboy", 300),
        ("Aphex Twin - Flim", 177),
        ("C418 - Sweden", 215),
        ("Emancipator - Anthem", 268),
        ("Nujabes - Aruarian Dance", 230),
        ("RJD2 - Ghostwriter", 233),
        ("The Avalanches - Since I Left You", 272),
        ("DJ Shadow - Midnight in a Perfect World", 290),
    ];
    raw.into_iter()
        .map(|(raw_title, secs)| {
            let (artist, title) = raw_title
                .split_once(" - ")
                .expect("fake titles look like `artist - title`");
            Track {
                id: Track::next_id(),
                source: TrackSource::Local(PathBuf::from(format!("/mock/{raw_title}.flac"))),
                artist: Some(artist.into()),
                title: Some(title.into()),
                duration: Some(Duration::from_secs(secs)),
                bitrate_kbps: None,
            }
        })
        .collect()
}

impl Default for MockPlayer {
    fn default() -> Self {
        Self::animated()
    }
}

impl MockPlayer {
    /// The live, animated mock: the spectrum wobbles every tick.
    pub fn animated() -> Self {
        let (spectrum, peaks, rng) = seeded_spectrum(ANIMATED_SEED);
        Self {
            state: PlayState::Playing,
            position: Duration::from_secs(84),
            playlist: Playlist::from_tracks(fake_tracks()),
            shuffle: false,
            repeat: false,
            volume: 68,
            balance: 0,
            format: FormatInfo::default(),
            spectrum,
            peaks,
            rng,
            frozen: false,
        }
        .with_current(2)
    }

    /// Frozen state for `--dump` and the golden tests: 1:24 into track 3 with
    /// a fixed spectrum. Only the clock advances on tick; the bars hold still
    /// so the output is reproducible.
    pub fn deterministic() -> Self {
        let (spectrum, peaks, rng) = seeded_spectrum(DETERMINISTIC_SEED);
        Self {
            state: PlayState::Playing,
            position: Duration::from_secs(84),
            playlist: Playlist::from_tracks(fake_tracks()),
            shuffle: false,
            repeat: false,
            volume: 68,
            balance: 0,
            format: FormatInfo::default(),
            spectrum,
            peaks,
            rng,
            frozen: true,
        }
        .with_current(2)
    }

    /// Seed-controlled mock for tests (the spectrum depends on the seed).
    #[cfg(test)]
    pub fn new(seed: u64) -> Self {
        let (spectrum, peaks, rng) = seeded_spectrum(seed);
        Self {
            state: PlayState::Playing,
            position: Duration::from_secs(84),
            playlist: Playlist::from_tracks(fake_tracks()),
            shuffle: false,
            repeat: false,
            volume: 68,
            balance: 0,
            format: FormatInfo::default(),
            spectrum,
            peaks,
            rng,
            frozen: false,
        }
        .with_current(2)
    }

    /// A stopped, empty-player mock: no tracks, nothing current (plan 02).
    /// Test-only since plan 03: real sessions use `RealPlayer::new()`.
    #[cfg(test)]
    pub fn empty() -> Self {
        let (spectrum, peaks, rng) = seeded_spectrum(ANIMATED_SEED);
        Self {
            state: PlayState::Stopped,
            position: Duration::ZERO,
            playlist: Playlist::new(),
            shuffle: false,
            repeat: false,
            volume: 68,
            balance: 0,
            format: FormatInfo::default(),
            spectrum,
            peaks,
            rng,
            frozen: false,
        }
    }

    fn with_current(mut self, current: usize) -> Self {
        self.playlist.set_current(Some(current));
        self
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            state: self.state,
            current: self.playlist.current(),
            position: self.position,
            tracks: self.playlist.tracks().clone(),
            shuffle: self.shuffle,
            repeat: self.repeat,
            volume: self.volume,
            balance: self.balance,
            format: self.format.clone(),
            spectrum: self.spectrum.clone(),
            peaks: self.peaks.clone(),
        }
    }

    pub fn handle(&mut self, cmd: PlayerCommand) {
        // Playlist-mutating commands work even when the list is empty.
        match cmd {
            PlayerCommand::AddTracks(new) => {
                self.playlist.append_sorted(new);
                return;
            }
            PlayerCommand::ReplaceTracks(new) => {
                self.playlist.replace(new);
                if self.playlist.is_empty() {
                    self.state = PlayState::Stopped;
                    self.position = Duration::ZERO;
                } else {
                    self.playlist.set_current(Some(0));
                    self.position = Duration::ZERO;
                    self.state = PlayState::Playing;
                }
                return;
            }
            PlayerCommand::RemoveTrack(i) => {
                if self.playlist.remove(i) {
                    // The current track was deleted: stop at zero.
                    self.state = PlayState::Stopped;
                    self.position = Duration::ZERO;
                }
                return;
            }
            PlayerCommand::ClearPlaylist => {
                self.playlist.clear();
                self.state = PlayState::Stopped;
                self.position = Duration::ZERO;
                return;
            }
            PlayerCommand::MoveTrack { from, up } => {
                if up {
                    self.playlist.move_up(from);
                } else {
                    self.playlist.move_down(from);
                }
                return;
            }
            // Volume/balance are stored only — the mock has no samples.
            PlayerCommand::SetVolume(v) => self.volume = v.min(100),
            PlayerCommand::SetBalance(b) => self.balance = b.clamp(-100, 100),
            _ => {}
        }
        self.handle_transport(cmd);
    }

    /// Transport and seek — everything that needs a non-empty playlist.
    fn handle_transport(&mut self, cmd: PlayerCommand) {
        let len = self.playlist.len();
        if len == 0 {
            return;
        }
        match cmd {
            PlayerCommand::Play => {
                if self.playlist.current().is_none() {
                    self.playlist.set_current(Some(0));
                    self.position = Duration::ZERO;
                }
                self.state = PlayState::Playing;
            }
            PlayerCommand::Pause => {
                if self.state == PlayState::Playing {
                    self.state = PlayState::Paused;
                }
            }
            PlayerCommand::Stop => {
                self.state = PlayState::Stopped;
                self.position = Duration::ZERO;
            }
            PlayerCommand::TogglePlayPause => match self.state {
                PlayState::Playing => self.handle_transport(PlayerCommand::Pause),
                _ => self.handle_transport(PlayerCommand::Play),
            },
            PlayerCommand::Next => {
                let next = self.playlist.current().map_or(0, |i| (i + 1) % len);
                self.playlist.set_current(Some(next));
                self.position = Duration::ZERO;
            }
            PlayerCommand::Prev => {
                // Winamp-style: restart the track unless we're near its start.
                if self.position > Duration::from_secs(3) {
                    self.position = Duration::ZERO;
                } else {
                    let prev = self
                        .playlist
                        .current()
                        .map_or(0, |i| i.checked_sub(1).unwrap_or(len - 1));
                    self.playlist.set_current(Some(prev));
                    self.position = Duration::ZERO;
                }
            }
            PlayerCommand::PlayTrack(i) if i < len => {
                self.playlist.set_current(Some(i));
                self.position = Duration::ZERO;
                self.state = PlayState::Playing;
            }
            PlayerCommand::PlayTrack(_) => {}
            PlayerCommand::SeekBy(delta) => {
                if let Some(track) = self.current_track() {
                    let track_len = track.duration.unwrap_or(UNKNOWN_DURATION).as_secs();
                    let target = self
                        .position
                        .as_secs()
                        .saturating_add_signed(delta)
                        .min(track_len);
                    self.position = Duration::from_secs(target);
                }
            }
            PlayerCommand::ToggleShuffle => self.shuffle = !self.shuffle,
            PlayerCommand::ToggleRepeat => self.repeat = !self.repeat,
            // Handled by `handle`.
            _ => {}
        }
    }

    fn current_track(&self) -> Option<&Track> {
        self.playlist
            .current()
            .and_then(|i| self.playlist.tracks().get(i))
    }

    /// Advance the clock, spectrum, and peaks. Called from `App::tick` with
    /// the elapsed time since the last frame.
    pub fn tick(&mut self, dt: Duration) {
        if self.state != PlayState::Playing {
            if !self.frozen {
                self.decay_spectrum();
            }
            return;
        }

        self.position += dt;
        if let Some(track) = self.current_track() {
            // Durations can be unknown → assume a plausible 3 minutes.
            let track_len = track.duration.unwrap_or(UNKNOWN_DURATION);
            if self.position >= track_len {
                if self.repeat {
                    self.position = Duration::ZERO;
                } else {
                    self.handle(PlayerCommand::Next);
                }
            }
        }

        // Frozen (dump/golden) mode: the clock moved but the bars hold still.
        if self.frozen {
            return;
        }

        // Spectrum: low bins louder than high bins, with a rolling wobble that
        // can drop to zero so bars fall all the way down.
        let playing = self.state == PlayState::Playing;
        let n = self.spectrum.len();
        for (i, bin) in self.spectrum.iter_mut().enumerate() {
            let freq_shape = 1.0 - (i as f32 / n as f32) * 0.7;
            let wobble = self.rng.next_f32();
            let target = if playing {
                (freq_shape * wobble * 1.15).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Attack fast, release moderately fast (bars can reach zero).
            if target > *bin {
                *bin = target;
            } else {
                *bin = (*bin - target).mul_add(0.45, target);
            }
            let peak = &mut self.peaks[i];
            if *bin >= *peak {
                *peak = *bin;
            } else {
                *peak = (*peak - *bin).mul_add(0.06, *bin).max(*bin);
            }
        }
    }

    fn decay_spectrum(&mut self) {
        for bin in &mut self.spectrum {
            *bin *= 0.75;
        }
    }

    /// Replace a track title (test support).
    #[cfg(test)]
    pub fn set_track_title(&mut self, index: usize, title: &str) {
        self.playlist.set_title(index, title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_playlist_is_in_range() {
        let p = MockPlayer::new(1);
        assert!((15..=50).contains(&p.playlist.len()));
        assert_eq!(p.snapshot().tracks.len(), p.playlist.len());
    }

    #[test]
    fn fake_tracks_render_their_original_text() {
        // Golden frames depend on `display_name` reproducing the raw string.
        let tracks = fake_tracks();
        assert_eq!(tracks[0].display_name(), "Aphex Twin - Xtal");
        assert_eq!(
            tracks[23].display_name(),
            "DJ Shadow - Midnight in a Perfect World"
        );
    }

    #[test]
    fn tick_advances_position_only_while_playing() {
        let mut p = MockPlayer::new(1);
        p.state = PlayState::Playing;
        let before = p.position;
        p.tick(Duration::from_secs(1));
        assert_eq!(p.position, before + Duration::from_secs(1));

        p.handle(PlayerCommand::Pause);
        let before = p.position;
        p.tick(Duration::from_secs(5));
        assert_eq!(p.position, before, "paused clock must not advance");
    }

    #[test]
    fn track_end_wraps_to_next() {
        let mut p = MockPlayer::new(1);
        p.playlist.set_current(Some(0));
        p.position = Duration::from_secs(290);
        p.tick(Duration::from_secs(2));
        assert_eq!(p.playlist.current(), Some(1));
        assert_eq!(p.position, Duration::ZERO);
    }

    #[test]
    fn commands_clamp() {
        let mut p = MockPlayer::new(1);
        p.handle(PlayerCommand::SetVolume(200));
        assert_eq!(p.volume, 100);
    }

    #[test]
    fn spectrum_stays_in_range() {
        let mut p = MockPlayer::new(7);
        for _ in 0..200 {
            p.tick(Duration::from_millis(500));
        }
        for bin in &p.spectrum {
            assert!((0.0..=1.0).contains(bin));
        }
    }

    #[test]
    fn empty_player_is_stopped_and_empty() {
        let mut p = MockPlayer::empty();
        assert_eq!(p.state, PlayState::Stopped);
        assert_eq!(p.playlist.current(), None);
        assert!(p.playlist.is_empty());
        assert_eq!(p.volume, 68);
        assert_eq!(p.balance, 0);
        // Transport on an empty list is a no-op, never a panic.
        p.handle(PlayerCommand::Next);
        p.handle(PlayerCommand::Play);
        p.tick(Duration::from_secs(100));
        assert_eq!(p.playlist.current(), None);
    }

    #[test]
    fn remove_current_track_stops_playback() {
        let mut p = MockPlayer::new(1);
        assert_eq!(p.playlist.current(), Some(2));
        p.handle(PlayerCommand::RemoveTrack(2));
        assert_eq!(p.state, PlayState::Stopped);
        assert_eq!(p.position, Duration::ZERO);
        assert_eq!(p.playlist.current(), None);

        // Removing a *non*-current track keeps the state and shifts current.
        let mut p = MockPlayer::new(1);
        p.handle(PlayerCommand::RemoveTrack(0));
        assert_eq!(p.state, PlayState::Playing);
        assert_eq!(p.playlist.current(), Some(1));
    }

    #[test]
    fn add_and_replace_commands_rewire_the_playlist() {
        let mut p = MockPlayer::new(1);
        let extra = fake_tracks()[0].clone();
        p.handle(PlayerCommand::AddTracks(vec![extra.clone()]));
        assert_eq!(p.playlist.len(), 25);

        p.handle(PlayerCommand::ReplaceTracks(vec![extra]));
        assert_eq!(p.playlist.len(), 1);
        assert_eq!(p.playlist.current(), Some(0), "replace starts at track 1");
        assert_eq!(p.state, PlayState::Playing);

        // Replace with nothing → stopped, empty.
        p.handle(PlayerCommand::ReplaceTracks(vec![]));
        assert!(p.playlist.is_empty());
        assert_eq!(p.state, PlayState::Stopped);
    }

    #[test]
    fn unknown_duration_falls_back_to_three_minutes() {
        // A playing track with no duration reaches its end after the assumed
        // 180 s → the player wraps to the next track (here: wraps to 0).
        let mut p = MockPlayer::new(1);
        let mut t = fake_tracks()[0].clone();
        t.duration = None;
        p.handle(PlayerCommand::ReplaceTracks(vec![t]));
        p.position = Duration::from_secs(179);
        p.tick(Duration::from_secs(2));
        assert_eq!(p.position, Duration::ZERO, "wrapped at the 180 s mark");
        assert_eq!(
            p.playlist.current(),
            Some(0),
            "single track wraps to itself"
        );

        // With a second track it advances instead of wrapping.
        let mut p = MockPlayer::new(1);
        let mut a = fake_tracks()[0].clone();
        a.duration = None;
        p.handle(PlayerCommand::ReplaceTracks(vec![
            a,
            fake_tracks()[1].clone(),
        ]));
        p.position = Duration::from_secs(180);
        p.tick(Duration::from_secs(1));
        assert_eq!(p.playlist.current(), Some(1));
    }
}
