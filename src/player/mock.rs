//! Mock player: fake playlist, ticking clock, animated spectrum.
//!
//! Stands in for the real audio thread until it exists. The RNG is seeded so
//! a given seed always yields the same spectrum; [`MockPlayer::deterministic`]
//! freezes that seed's spectrum so `--dump` and the golden tests reproduce
//! exactly.

use std::time::Duration;

use super::{FormatInfo, PlayState, PlayerCommand, PlayerSnapshot, Track};

/// Tiny xorshift PRNG — enough for spectrum wobble, no dependency.
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

const SPECTRUM_BINS: usize = 24;

/// Seed for the animated (non-deterministic) player.
const ANIMATED_SEED: u64 = 42;
/// Seed for the frozen `--dump` / golden-test spectrum.
const DETERMINISTIC_SEED: u64 = 0x5EED_C0DE;

pub struct MockPlayer {
    state: PlayState,
    current: Option<usize>,
    position: Duration,
    tracks: Vec<Track>,
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
    let mut rng = Rng(seed | 1);
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

/// 24 fake tracks of imaginary downtempo.
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
        .map(|(title, secs)| Track {
            title: title.into(),
            duration: Duration::from_secs(secs),
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
            current: Some(2),
            position: Duration::from_secs(84),
            tracks: fake_tracks(),
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

    /// Frozen state for `--dump` and the golden tests: 1:24 into track 3 with
    /// a fixed spectrum. Only the clock advances on tick; the bars hold still
    /// so the output is reproducible.
    pub fn deterministic() -> Self {
        let (spectrum, peaks, rng) = seeded_spectrum(DETERMINISTIC_SEED);
        Self {
            state: PlayState::Playing,
            current: Some(2),
            position: Duration::from_secs(84),
            tracks: fake_tracks(),
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
    }

    /// Seed-controlled mock for tests (the spectrum depends on the seed).
    #[cfg(test)]
    pub fn new(seed: u64) -> Self {
        let (spectrum, peaks, rng) = seeded_spectrum(seed);
        Self {
            state: PlayState::Playing,
            current: Some(2),
            position: Duration::from_secs(84),
            tracks: fake_tracks(),
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

    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            state: self.state,
            current: self.current,
            position: self.position,
            tracks: self.tracks.clone(),
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
        let len = self.tracks.len();
        if len == 0 {
            return;
        }
        match cmd {
            PlayerCommand::Play => {
                if self.current.is_none() {
                    self.current = Some(0);
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
                PlayState::Playing => self.handle(PlayerCommand::Pause),
                _ => self.handle(PlayerCommand::Play),
            },
            PlayerCommand::Next => {
                let next = self.current.map_or(0, |i| (i + 1) % len);
                self.current = Some(next);
                self.position = Duration::ZERO;
            }
            PlayerCommand::Prev => {
                // Winamp-style: restart the track unless we're near its start.
                if self.position > Duration::from_secs(3) {
                    self.position = Duration::ZERO;
                } else {
                    let prev = self
                        .current
                        .map_or(0, |i| i.checked_sub(1).unwrap_or(len - 1));
                    self.current = Some(prev);
                    self.position = Duration::ZERO;
                }
            }
            PlayerCommand::PlayTrack(i) if i < len => {
                self.current = Some(i);
                self.position = Duration::ZERO;
                self.state = PlayState::Playing;
            }
            PlayerCommand::PlayTrack(_) => {}
            PlayerCommand::SeekBy(delta) => {
                if let Some(track) = self.current_track() {
                    let track_len = track.duration.as_secs();
                    let target = self
                        .position
                        .as_secs()
                        .saturating_add_signed(delta)
                        .min(track_len);
                    self.position = Duration::from_secs(target);
                }
            }
            PlayerCommand::SetVolume(v) => self.volume = v.min(100),
            PlayerCommand::ToggleShuffle => self.shuffle = !self.shuffle,
            PlayerCommand::ToggleRepeat => self.repeat = !self.repeat,
        }
    }

    fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.tracks.get(i))
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
        if let Some(track) = self.current_track()
            && self.position >= track.duration
        {
            if self.repeat {
                self.position = Duration::ZERO;
            } else {
                self.handle(PlayerCommand::Next);
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
        if let Some(track) = self.tracks.get_mut(index) {
            track.title = title.into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_playlist_is_in_range() {
        let p = MockPlayer::new(1);
        assert!((15..=50).contains(&p.tracks.len()));
        assert_eq!(p.snapshot().tracks.len(), p.tracks.len());
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
        p.current = Some(0);
        p.position = Duration::from_secs(290);
        p.tick(Duration::from_secs(2));
        assert_eq!(p.current, Some(1));
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
}
