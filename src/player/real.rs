//! The real player: playlist + play order + the audio engine (plan 03),
//! with the clock, seeking and the FFT spectrum in sync with the audio
//! (plan 04).
//!
//! Talks to the engine through the [`AudioOut`] trait — production wields the
//! [`Engine`] thread, tests inject a fake that records commands and replays
//! events — so every state-machine rule here is testable without an audio
//! device.
//!
//! Sync design (plan 04): the position is `start_offset + frames the audio
//! callback really delivered` — never wall time, so it cannot drift, and it
//! freezes by itself while paused or buffering. The spectrum runs a 2048-point
//! FFT ([`Analyzer`]) over a mono sample tap on this (UI) thread, with the
//! mock's attack/release feel.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::audio::{
    Analyzer, AudioOut, BANDS, Engine, EngineCmd, EngineEvent, PlaybackHandle, SEEK_UNSUPPORTED,
    StreamInfo,
};

use super::order::PlayOrder;
use super::playlist::Playlist;
use super::rng::Rng;
use super::{FormatInfo, PlayState, PlayerCommand, PlayerSnapshot, TrackSource};

/// Only the last `TAP_ANALYZE` tapped samples feed one FFT frame.
const TAP_ANALYZE: usize = 4096;
/// Bars fall to zero at this rate per frame while paused (the mock's rule).
const PAUSE_DECAY: f32 = 0.75;
/// Release: bars fall `bin = target + (bin - target) * RELEASE` (mock copy).
const RELEASE: f32 = 0.45;
/// Peak caps fall at this factor per frame (mock copy).
const PEAK_FALL: f32 = 0.06;

pub struct RealPlayer {
    playlist: Playlist,
    state: PlayState,
    shuffle: bool,
    repeat: bool,
    /// Stored only until plan 05 applies it to the samples.
    volume: u8,
    /// Stored only until plan 05 applies it to the samples.
    balance: i8,
    /// From [`EngineEvent::Started`]; blank while stopped.
    format: FormatInfo,
    /// Live clock + sample tap from the engine; `None` while stopped.
    handle: Option<PlaybackHandle>,
    /// Where the in-flight `Load` should start reading. Shown as the
    /// position until the engine's `Started` replaces the handle, so a
    /// seek jumps the clock instantly (no `0:00` flash) and a second seek
    /// right after the first still counts from the new offset.
    pending_start: Option<Duration>,
    /// FFT over the sample tap; only touched while playing. Boxed: the
    /// analyzer's window + ring are ~16 KB, too big to sit inline in a
    /// command enum by value.
    analyzer: Box<Analyzer>,
    /// Reused tap-drain buffer (no per-frame allocation).
    tap_buf: Vec<f32>,
    /// Smoothed band levels / peak caps, 0.0–1.0 (plan 04: 32 real bands).
    spectrum: Vec<f32>,
    peaks: Vec<f32>,
    /// The engine seam: production [`Engine`], a recording fake in tests.
    engine: Box<dyn AudioOut>,
    order: PlayOrder,
    /// Consecutive [`EngineEvent::Error`]s: stops after they span the list.
    fail_streak: usize,
    notice: Option<String>,
    /// The shuffle source (xorshift, seeded from the wall clock).
    rng: Rng,
}

impl RealPlayer {
    /// The production player: an engine thread that owns the audio device.
    pub fn new() -> Self {
        Self::with_engine(Box::new(Engine::spawn()))
    }

    /// Same state machine over a different [`AudioOut`] (tests).
    fn with_engine(engine: Box<dyn AudioOut>) -> Self {
        Self {
            playlist: Playlist::new(),
            state: PlayState::Stopped,
            shuffle: false,
            repeat: false,
            // Full volume: no DSP exists yet, so samples pass through
            // untouched (plans-README rule 7). Plan 05 makes this live.
            volume: 100,
            balance: 0,
            format: FormatInfo::blank(),
            handle: None,
            pending_start: None,
            analyzer: Box::new(Analyzer::new()),
            tap_buf: Vec::new(),
            spectrum: vec![0.0; BANDS],
            peaks: vec![0.0; BANDS],
            engine,
            order: PlayOrder::new(),
            fail_streak: 0,
            notice: None,
            rng: Rng::new(wall_clock_seed()),
        }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            state: self.state,
            current: self.playlist.current(),
            position: self.position(),
            tracks: self.playlist.tracks().clone(),
            shuffle: self.shuffle,
            repeat: self.repeat,
            volume: self.volume,
            balance: self.balance,
            format: if self.state == PlayState::Stopped {
                FormatInfo::blank()
            } else {
                self.format.clone()
            },
            spectrum: self.spectrum.clone(),
            peaks: self.peaks.clone(),
        }
    }

    /// Where the music is right now: the seek offset plus the frames the
    /// callback really delivered, clamped to the duration when known. While
    /// a `Load` is still in flight (no handle yet), the pending start offset
    /// stands in, so seeks jump the clock instantly.
    fn position(&self) -> Duration {
        let position = match (&self.handle, self.pending_start) {
            (Some(handle), _) => position(
                handle.start_offset,
                handle.played.load(Ordering::Relaxed),
                handle.sample_rate,
            ),
            (None, Some(start)) => start,
            (None, None) => return Duration::ZERO, // stopped: no clock
        };
        let Some(track) = self
            .playlist
            .current()
            .and_then(|i| self.playlist.tracks().get(i))
        else {
            return position;
        };
        match track.duration {
            Some(duration) => position.min(duration),
            None => position,
        }
    }

    /// The next queued user-facing notice (`skipped …`, `shuffle on`, …).
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    pub fn handle(&mut self, cmd: PlayerCommand) {
        // Playlist-mutating commands work even when the list is empty.
        match cmd {
            PlayerCommand::AddTracks(new) => {
                self.playlist.append_sorted(new);
                self.rebuild_order();
                return;
            }
            PlayerCommand::ReplaceTracks(new) => {
                self.playlist.replace(new);
                if self.playlist.is_empty() {
                    self.stop_playback();
                    self.rebuild_order();
                } else {
                    // Behaves like `PlayTrack(0)` (plan §step 5).
                    self.playlist.set_current(Some(0));
                    self.rebuild_order();
                    self.start_track(0);
                }
                return;
            }
            PlayerCommand::RemoveTrack(i) => {
                let was_current = self.playlist.remove(i);
                self.rebuild_order();
                if was_current {
                    self.stop_playback();
                }
                return;
            }
            PlayerCommand::ClearPlaylist => {
                self.playlist.clear();
                self.rebuild_order();
                self.stop_playback();
                return;
            }
            PlayerCommand::MoveTrack { from, up } => {
                if up {
                    self.playlist.move_up(from);
                } else {
                    self.playlist.move_down(from);
                }
                self.rebuild_order();
                return;
            }
            _ => {}
        }
        self.handle_transport(cmd);
    }

    /// Transport — everything that needs a non-empty playlist.
    fn handle_transport(&mut self, cmd: PlayerCommand) {
        if self.playlist.is_empty() {
            return;
        }
        match cmd {
            PlayerCommand::Play => match self.state {
                PlayState::Paused => {
                    self.engine.send(EngineCmd::Resume);
                    self.state = PlayState::Playing;
                }
                PlayState::Playing => {}
                PlayState::Stopped => {
                    if let Some(i) = self.playlist.current() {
                        self.start_track(i);
                    } else {
                        // Nothing current: start at playlist track 1 and put
                        // the order's cursor on it (otherwise the first
                        // auto-advance would replay it).
                        self.order.jump_to(0);
                        self.start_track(0);
                    }
                }
            },
            PlayerCommand::Pause => {
                if self.state == PlayState::Playing {
                    self.engine.send(EngineCmd::Pause);
                    self.state = PlayState::Paused;
                }
            }
            PlayerCommand::Stop => self.stop_playback(),
            PlayerCommand::TogglePlayPause => match self.state {
                PlayState::Playing => self.handle_transport(PlayerCommand::Pause),
                _ => self.handle_transport(PlayerCommand::Play),
            },
            // End of track and manual Next share one rule (plan §Decisions).
            PlayerCommand::Next => self.advance_after_end(),
            PlayerCommand::Prev => {
                if self.position() > Duration::from_secs(3) {
                    // Past 3 s: restart this track from 0 (plan §step 4).
                    self.reload_current();
                } else if let Some(i) = self.order.prev() {
                    self.start_track(i);
                }
            }
            PlayerCommand::PlayTrack(i) if i < self.playlist.len() => {
                self.order.jump_to(i);
                self.start_track(i);
            }
            PlayerCommand::PlayTrack(_) => {}
            PlayerCommand::SeekBy(delta) => self.seek_by(delta),
            PlayerCommand::SetVolume(v) => self.volume = v.min(100),
            PlayerCommand::ToggleShuffle => {
                self.shuffle = !self.shuffle;
                self.rebuild_order();
                self.notice = Some(
                    if self.shuffle {
                        "shuffle on"
                    } else {
                        "shuffle off"
                    }
                    .into(),
                );
            }
            PlayerCommand::ToggleRepeat => {
                self.repeat = !self.repeat;
                self.notice = Some(
                    if self.repeat {
                        "repeat on"
                    } else {
                        "repeat off"
                    }
                    .into(),
                );
            }
            // Handled by `handle`.
            _ => {}
        }
    }

    /// `←`/`→` (∓5 s): reload the same track at a new offset. Seeking past
    /// the end stops one second early so it can never skip the track.
    fn seek_by(&mut self, delta: i64) {
        if !matches!(self.state, PlayState::Playing | PlayState::Paused) {
            return; // stopped: nothing to seek
        }
        let Some(i) = self.playlist.current() else {
            return;
        };
        let mut target = (self.position().as_secs_f64() + delta as f64).max(0.0);
        if let Some(duration) = self.playlist.tracks().get(i).and_then(|t| t.duration) {
            target = target.min((duration.as_secs_f64() - 1.0).max(0.0));
        }
        let keep_paused = self.state == PlayState::Paused;
        self.load_track(i, Duration::from_secs_f64(target), keep_paused);
    }

    /// Advance the clock and the bars: drain engine events into UI state,
    /// then re-smooth the spectrum (plan §step 6).
    pub fn tick(&mut self, _dt: Duration) {
        for event in self.engine.poll() {
            match event {
                EngineEvent::Started(info, handle) => self.on_started(info, handle),
                EngineEvent::Finished => self.advance_after_end(),
                EngineEvent::Error(reason) => self.on_error(reason),
            }
        }
        self.update_spectrum();
    }

    /// The track opened: clock + tap handle, hi-fi line, streak reset, and
    /// the decoder's accurate duration when the tag had none or lied (the
    /// tag is off by more than a second).
    fn on_started(&mut self, info: StreamInfo, handle: PlaybackHandle) {
        self.fail_streak = 0;
        if let Some(decoded) = info.duration {
            let tagged = self
                .playlist
                .current()
                .and_then(|i| self.playlist.tracks().get(i))
                .and_then(|t| t.duration);
            let tag_lies = tagged
                .is_none_or(|tagged| (tagged.as_secs_f64() - decoded.as_secs_f64()).abs() > 1.0);
            if tag_lies && let Some(i) = self.playlist.current() {
                self.playlist.set_duration(i, decoded);
            }
        }
        self.handle = Some(handle);
        self.pending_start = None; // the live clock takes over
        self.format = FormatInfo {
            codec: info.codec,
            // Lossless source, no resampling (the device opens at the track's
            // own rate), no DSP yet → the samples reach the device unchanged.
            // plan 05: goes false once volume/balance/EQ act on them.
            bit_perfect: info.lossless,
            bit_depth: info.bit_depth,
            sample_rate: Some(info.sample_rate),
            channels: channels_label(info.channels),
        };
    }

    /// A track could not be played: notice + skip to the next one, or stop
    /// once the failures span the whole playlist. A failed *seek* is not a
    /// bad file: restart the same track from 0 with a notice instead.
    fn on_error(&mut self, reason: String) {
        if reason == SEEK_UNSUPPORTED {
            self.notice = Some("seek not supported for this file".into());
            self.reload_current();
            return;
        }
        self.fail_streak += 1;
        let name = self
            .playlist
            .current()
            .and_then(|i| self.playlist.tracks().get(i))
            .map(|t| t.display_name())
            .unwrap_or_else(|| "track".into());
        self.notice = Some(format!("skipped {name}: {reason}"));
        if self.fail_streak >= self.playlist.len() {
            self.stop_playback();
            self.notice = Some("no playable tracks".into());
        } else {
            self.advance_after_end();
        }
    }

    /// End of track / manual Next: the play order decides, stopping when it
    /// has nothing left (repeat off).
    fn advance_after_end(&mut self) {
        match self.order.next(self.repeat, &mut self.rng) {
            Some(i) => self.start_track(i),
            None => self.stop_playback(),
        }
    }

    /// Load a track from the start and mark it current + playing.
    fn start_track(&mut self, i: usize) {
        self.load_track(i, Duration::ZERO, false);
    }

    /// Reload the current track from 0 (Prev past 3 s, unsupported seek).
    fn reload_current(&mut self) {
        if let Some(i) = self.playlist.current() {
            let keep_paused = self.state == PlayState::Paused;
            self.load_track(i, Duration::ZERO, keep_paused);
        }
    }

    /// Load track `i` into the engine at `start`. The engine's pause flag
    /// survives a `Load` (that is what makes a paused seek race-free), so
    /// every load re-asserts the state it wants: `Pause` to stay paused,
    /// `Resume` to make sure a stale pause never mutes a fresh track.
    fn load_track(&mut self, i: usize, start: Duration, keep_paused: bool) {
        let Some(TrackSource::Local(path)) = self
            .playlist
            .tracks()
            .get(i)
            .map(|track| track.source.clone())
        else {
            return;
        };
        self.playlist.set_current(Some(i));
        self.state = if keep_paused {
            PlayState::Paused
        } else {
            PlayState::Playing
        };
        // The old handle's clock dies with the old stream; the pending
        // offset stands in until the engine reports `Started`.
        self.handle = None;
        self.pending_start = Some(start);
        self.engine.send(EngineCmd::Load { path, start });
        if keep_paused {
            self.engine.send(EngineCmd::Pause);
        } else {
            self.engine.send(EngineCmd::Resume);
        }
    }

    /// Halt playback but keep the current track selected (Stop semantics).
    fn stop_playback(&mut self) {
        self.engine.send(EngineCmd::Stop);
        self.state = PlayState::Stopped;
        self.format = FormatInfo::blank();
        self.handle = None;
        self.pending_start = None;
    }

    /// Smooth the analyzer's band levels into the bars (plan §step 6):
    /// instant attack, release `0.45`, peak caps falling at `0.06` — the
    /// exact constants of `MockPlayer::tick`. Paused decays the bars
    /// (`×0.75`), stopped zeroes everything.
    fn update_spectrum(&mut self) {
        match self.state {
            PlayState::Stopped => {
                self.spectrum.iter_mut().for_each(|bin| *bin = 0.0);
                self.peaks.iter_mut().for_each(|cap| *cap = 0.0);
            }
            PlayState::Paused => {
                for bin in &mut self.spectrum {
                    *bin *= PAUSE_DECAY;
                }
            }
            PlayState::Playing => {
                let mut target = [0.0; BANDS];
                if let Some(handle) = &mut self.handle {
                    // Drain the tap completely; only the newest samples feed
                    // one FFT frame (the ring is bigger than a frame).
                    self.tap_buf.clear();
                    while let Ok(sample) = handle.tap.pop() {
                        self.tap_buf.push(sample);
                    }
                    if self.tap_buf.len() > TAP_ANALYZE {
                        let excess = self.tap_buf.len() - TAP_ANALYZE;
                        self.tap_buf.drain(..excess);
                    }
                    self.analyzer.push(&self.tap_buf);
                    self.analyzer.bands(handle.sample_rate, &mut target);
                }
                for (i, bin) in self.spectrum.iter_mut().enumerate() {
                    let target = target[i];
                    // Attack fast, release moderately fast (mock copy).
                    if target > *bin {
                        *bin = target;
                    } else {
                        *bin = (*bin - target).mul_add(RELEASE, target);
                    }
                    let cap = &mut self.peaks[i];
                    if *bin >= *cap {
                        *cap = *bin;
                    } else {
                        *cap = (*cap - *bin).mul_add(PEAK_FALL, *bin).max(*bin);
                    }
                }
            }
        }
    }

    /// Keep the play order in step with the playlist (and the shuffle flag)
    /// after any edit or shuffle toggle.
    fn rebuild_order(&mut self) {
        let current = self.playlist.current();
        self.order
            .rebuild(self.playlist.len(), current, self.shuffle, &mut self.rng);
    }
}

impl Default for RealPlayer {
    fn default() -> Self {
        Self::new()
    }
}

/// `mono` / `stereo` / `{n}ch`, the way the format line shows it.
fn channels_label(channels: u16) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        n => format!("{n}ch"),
    }
}

/// The playback position: where the decoder landed plus the frames the
/// audio callback really delivered (plan §step 1 — frames, never wall time,
/// so the clock cannot drift or run while buffering).
fn position(start_offset: Duration, played_frames: u64, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        return start_offset;
    }
    start_offset + Duration::from_secs_f64(played_frames as f64 / sample_rate as f64)
}

/// Wall-clock seed for shuffle (tests build players through `with_engine` and
/// seed explicitly where it matters).
fn wall_clock_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    use rtrb::RingBuffer;

    use super::*;
    use crate::player::{FormatInfo, Track};

    /// A fake [`AudioOut`]: records every command, hands back whatever events
    /// the test queued. Shared handles let the test inspect both sides.
    struct FakeEngine {
        cmds: Arc<Mutex<Vec<EngineCmd>>>,
        events: Arc<Mutex<Vec<EngineEvent>>>,
    }

    impl AudioOut for FakeEngine {
        fn send(&self, cmd: EngineCmd) {
            self.cmds.lock().expect("cmds lock").push(cmd);
        }

        fn poll(&self) -> Vec<EngineEvent> {
            std::mem::take(&mut *self.events.lock().expect("events lock"))
        }
    }

    /// `(player, command log, event queue)`.
    type Harness = (
        RealPlayer,
        Arc<Mutex<Vec<EngineCmd>>>,
        Arc<Mutex<Vec<EngineEvent>>>,
    );

    fn fake_player() -> Harness {
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let engine = FakeEngine {
            cmds: Arc::clone(&cmds),
            events: Arc::clone(&events),
        };
        (RealPlayer::with_engine(Box::new(engine)), cmds, events)
    }

    fn tracks(n: usize) -> Vec<Track> {
        (0..n)
            .map(|i| Track {
                id: Track::next_id(),
                source: TrackSource::Local(PathBuf::from(format!("/m/{i}.flac"))),
                artist: Some("A".into()),
                title: Some(format!("Track {i}")),
                duration: None,
            })
            .collect()
    }

    fn log_of(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<EngineCmd> {
        cmds.lock().expect("cmds lock").clone()
    }

    /// Every `Load`, as `(path, start)` pairs.
    fn loads_of(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<(PathBuf, Duration)> {
        log_of(cmds)
            .into_iter()
            .filter_map(|cmd| match cmd {
                EngineCmd::Load { path, start } => Some((path, start)),
                _ => None,
            })
            .collect()
    }

    fn load_paths(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<PathBuf> {
        loads_of(cmds).into_iter().map(|(path, _)| path).collect()
    }

    fn queue(events: &Arc<Mutex<Vec<EngineEvent>>>, event: EngineEvent) {
        events.lock().expect("events lock").push(event);
    }

    /// A 3-track list, no track playing yet (Add does not autoplay).
    fn three_tracks() -> Harness {
        let (mut player, cmds, events) = fake_player();
        player.handle(PlayerCommand::AddTracks(tracks(3)));
        assert!(loads_of(&cmds).is_empty(), "Add must not autoplay");
        (player, cmds, events)
    }

    /// `duration: None` so `on_started` leaves the playlist alone; seek
    /// tests that need a known length override it with `..fake_started()`.
    fn fake_started() -> StreamInfo {
        StreamInfo {
            codec: "FLAC".into(),
            lossless: true,
            sample_rate: 44_100,
            channels: 2,
            bit_depth: Some(16),
            duration: None,
        }
    }

    /// A handle reporting `played_frames` frames into the stream.
    fn fake_handle(played_frames: u64) -> PlaybackHandle {
        PlaybackHandle {
            played: Arc::new(AtomicU64::new(played_frames)),
            sample_rate: 44_100,
            start_offset: Duration::ZERO,
            tap: RingBuffer::<f32>::new(64).1,
        }
    }

    /// `Started` for a stream `played_frames` in (the common test event).
    fn fake_started_at(played_frames: u64) -> EngineEvent {
        EngineEvent::Started(fake_started(), fake_handle(played_frames))
    }

    // --- transport ---

    #[test]
    fn play_on_a_three_track_list_loads_track_zero() {
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::Play);
        assert_eq!(load_paths(&cmds), [PathBuf::from("/m/0.flac")]);
        assert_eq!(player.state, PlayState::Playing);
        assert_eq!(player.playlist.current(), Some(0));
        // Play while playing: no second load.
        player.handle(PlayerCommand::Play);
        assert_eq!(load_paths(&cmds).len(), 1);
        // Pause / resume hit the engine.
        player.handle(PlayerCommand::Pause);
        assert_eq!(player.state, PlayState::Paused);
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Pause)));
        player.handle(PlayerCommand::TogglePlayPause);
        assert_eq!(player.state, PlayState::Playing);
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Resume)));
        // Stop halts but keeps the selection.
        player.handle(PlayerCommand::Stop);
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(player.playlist.current(), Some(0));
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Stop)));
    }

    #[test]
    fn finished_advances_to_the_next_track() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, EngineEvent::Finished);
        player.tick(Duration::ZERO);
        assert_eq!(
            load_paths(&cmds),
            [PathBuf::from("/m/0.flac"), PathBuf::from("/m/1.flac")],
            "Finished loads track 2"
        );
        assert_eq!(player.playlist.current(), Some(1));
        assert_eq!(player.state, PlayState::Playing);
    }

    #[test]
    fn finished_on_the_last_track_stops_when_repeat_is_off() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::PlayTrack(2));
        queue(&events, EngineEvent::Finished);
        player.tick(Duration::ZERO);
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(player.playlist.current(), Some(2), "selection is kept");
        assert_eq!(
            load_paths(&cmds),
            [PathBuf::from("/m/2.flac")],
            "no more loads"
        );
        // Manual Next on the last track follows the same rule → still stopped.
        player.handle(PlayerCommand::Next);
        assert_eq!(player.state, PlayState::Stopped);
    }

    #[test]
    fn repeat_on_wraps_back_to_the_first_track() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::ToggleRepeat);
        player.handle(PlayerCommand::PlayTrack(2));
        queue(&events, EngineEvent::Finished);
        player.tick(Duration::ZERO);
        assert_eq!(player.state, PlayState::Playing);
        assert_eq!(player.playlist.current(), Some(0));
        assert_eq!(load_paths(&cmds).last(), Some(&PathBuf::from("/m/0.flac")));
    }

    #[test]
    fn prev_past_three_seconds_restarts_the_track_otherwise_steps_back() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::PlayTrack(1));
        // 4 s into track 2 → Prev restarts it (same track, `start: 0`).
        queue(&events, fake_started_at(4 * 44_100));
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::Prev);
        assert_eq!(player.playlist.current(), Some(1), "restart, not back");
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(PathBuf::from("/m/1.flac"), Duration::ZERO)),
            "restart issues another Load at 0"
        );
        // Back under 3 s (a fresh stream at 0) → Prev steps back.
        queue(&events, fake_started_at(44_100));
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::Prev);
        assert_eq!(player.playlist.current(), Some(0));
        // First track → restart it (same track again).
        player.handle(PlayerCommand::Prev);
        assert_eq!(player.playlist.current(), Some(0));
    }

    #[test]
    fn seek_forward_sends_a_load_with_the_new_offset() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        // 10 s into a 60 s track.
        queue(
            &events,
            EngineEvent::Started(
                StreamInfo {
                    duration: Some(Duration::from_secs(60)),
                    ..fake_started()
                },
                fake_handle(10 * 44_100),
            ),
        );
        player.tick(Duration::ZERO);
        assert_eq!(player.snapshot().position, Duration::from_secs(10));

        player.handle(PlayerCommand::SeekBy(5));
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(PathBuf::from("/m/0.flac"), Duration::from_secs(15))),
            "SeekBy(5) reloads at position + 5 s"
        );
        player.handle(PlayerCommand::SeekBy(-20));
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(PathBuf::from("/m/0.flac"), Duration::from_secs(0))),
            "seeking before the start clamps to 0"
        );
    }

    #[test]
    fn seeking_past_the_end_stops_one_second_short() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(
            &events,
            EngineEvent::Started(
                StreamInfo {
                    duration: Some(Duration::from_secs(60)),
                    ..fake_started()
                },
                fake_handle(55 * 44_100),
            ),
        );
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::SeekBy(30));
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(PathBuf::from("/m/0.flac"), Duration::from_secs(59))),
            "never seeks to the very end (would skip the track)"
        );
    }

    #[test]
    fn seeking_while_paused_sends_pause_right_after_the_load() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(2 * 44_100));
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::Pause);
        player.handle(PlayerCommand::SeekBy(5));

        let log = log_of(&cmds);
        let load_idx = log
            .iter()
            .rposition(|cmd| matches!(cmd, EngineCmd::Load { .. }))
            .expect("seek sends a Load");
        assert!(
            matches!(log.get(load_idx + 1), Some(EngineCmd::Pause)),
            "Pause must follow the seek Load: {log:?}"
        );
        assert_eq!(player.state, PlayState::Paused, "stays paused");
    }

    #[test]
    fn a_second_seek_before_started_counts_from_the_new_offset() {
        // Two `→` presses land within one engine round-trip: the second
        // must build on the first's target, not on a zeroed clock.
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::SeekBy(5));
        // No Started in between: the clock already shows the seek target.
        assert_eq!(player.snapshot().position, Duration::from_secs(5));
        player.handle(PlayerCommand::SeekBy(5));
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(PathBuf::from("/m/0.flac"), Duration::from_secs(10)))
        );
    }

    #[test]
    fn seek_is_ignored_while_stopped() {
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::SeekBy(30));
        assert!(loads_of(&cmds).is_empty(), "nothing to seek when stopped");
    }

    #[test]
    fn an_unsupported_seek_restarts_the_track_with_a_notice() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(10 * 44_100));
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::SeekBy(5));
        queue(&events, EngineEvent::Error(SEEK_UNSUPPORTED.into()));
        player.tick(Duration::ZERO);
        assert_eq!(
            player.take_notice().as_deref(),
            Some("seek not supported for this file")
        );
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(PathBuf::from("/m/0.flac"), Duration::ZERO)),
            "same track restarted from 0, not skipped"
        );
        assert_eq!(player.playlist.current(), Some(0));
        assert_eq!(player.state, PlayState::Playing);
    }

    // --- errors and notices ---

    #[test]
    fn error_notices_and_skips_to_the_next_track() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, EngineEvent::Error("cannot decode".into()));
        player.tick(Duration::ZERO);
        assert_eq!(
            player.take_notice().as_deref(),
            Some("skipped A - Track 0: cannot decode")
        );
        assert_eq!(load_paths(&cmds).last(), Some(&PathBuf::from("/m/1.flac")));
        assert_eq!(player.state, PlayState::Playing);
        // The notice is consumed exactly once.
        assert!(player.take_notice().is_none(), "notice consumed once");
    }

    #[test]
    fn a_full_streak_of_errors_stops_with_no_playable_tracks() {
        let (mut player, cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        for _ in 0..3 {
            queue(&events, EngineEvent::Error("bad file".into()));
            player.tick(Duration::ZERO);
        }
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(load_paths(&cmds).len(), 3, "two skips, then give up");
        assert_eq!(player.notice.as_deref(), Some("no playable tracks"));
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Stop)));
    }

    #[test]
    fn started_fills_the_format_line_and_blanks_it_on_stop() {
        let (mut player, _cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        assert_eq!(
            player.snapshot().format,
            FormatInfo::blank(),
            "nothing known before Started"
        );
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        let format = player.snapshot().format;
        assert_eq!(format.codec, "FLAC");
        assert_eq!(format.bit_depth, Some(16));
        assert_eq!(format.sample_rate, Some(44_100));
        assert_eq!(format.channels, "stereo");
        assert!(format.bit_perfect, "lossless → bit-perfect flag");
        // Lossy start → no bit-perfect flag.
        let mut lossy = fake_started();
        lossy.codec = "MP3".into();
        lossy.lossless = false;
        lossy.bit_depth = None;
        lossy.channels = 1;
        queue(&events, EngineEvent::Started(lossy, fake_handle(0)));
        player.tick(Duration::ZERO);
        let format = player.snapshot().format;
        assert_eq!(format.codec, "MP3");
        assert!(!format.bit_perfect);
        assert_eq!(format.channels, "mono");
        // Stop → blank again.
        player.handle(PlayerCommand::Stop);
        assert_eq!(player.snapshot().format, FormatInfo::blank());
    }

    #[test]
    fn the_clock_counts_frames_and_the_spectrum_has_32_bars() {
        let (mut player, _cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        // 2.5 s into the stream: 110 250 frames at 44.1 kHz.
        queue(&events, fake_started_at(110_250));
        player.tick(Duration::ZERO);
        let snapshot = player.snapshot();
        assert_eq!(snapshot.position, Duration::from_millis(2_500));
        assert_eq!(snapshot.spectrum.len(), 32);
        assert!(snapshot.spectrum.iter().all(|v| *v == 0.0));
        assert!(snapshot.peaks.iter().all(|v| *v == 0.0));
        // Stopped → the clock zeroes too.
        player.handle(PlayerCommand::Stop);
        assert_eq!(player.snapshot().position, Duration::ZERO);
    }

    #[test]
    fn position_is_clamped_to_the_known_track_duration() {
        let (mut player, _cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(
            &events,
            EngineEvent::Started(
                StreamInfo {
                    duration: Some(Duration::from_secs(30)),
                    ..fake_started()
                },
                // 40 s of frames into a 30 s track: clamped, never past it.
                fake_handle(40 * 44_100),
            ),
        );
        player.tick(Duration::ZERO);
        assert_eq!(player.snapshot().position, Duration::from_secs(30));
    }

    #[test]
    fn the_decoder_duration_overrides_a_missing_tag() {
        let (mut player, _cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        assert_eq!(player.playlist.tracks()[0].duration, None, "untagged");
        queue(
            &events,
            EngineEvent::Started(
                StreamInfo {
                    duration: Some(Duration::from_secs(245)),
                    ..fake_started()
                },
                fake_handle(0),
            ),
        );
        player.tick(Duration::ZERO);
        assert_eq!(
            player.playlist.tracks()[0].duration,
            Some(Duration::from_secs(245)),
            "decoder duration lands in the playlist"
        );
    }

    // --- playlist edits mirror the mock's semantics ---

    #[test]
    fn replace_autoplays_track_zero_and_empties_to_stopped() {
        let (mut player, cmds, _events) = fake_player();
        player.handle(PlayerCommand::ReplaceTracks(tracks(2)));
        assert_eq!(player.state, PlayState::Playing);
        assert_eq!(player.playlist.current(), Some(0));
        assert_eq!(load_paths(&cmds), [PathBuf::from("/m/0.flac")]);

        player.handle(PlayerCommand::ReplaceTracks(vec![]));
        assert_eq!(player.state, PlayState::Stopped);
        assert!(player.playlist.is_empty());
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Stop)));
    }

    #[test]
    fn removing_the_current_track_stops_and_removing_another_does_not() {
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::PlayTrack(1));
        let loads = load_paths(&cmds).len();

        player.handle(PlayerCommand::RemoveTrack(0));
        assert_eq!(player.state, PlayState::Playing, "still playing track 2");
        assert_eq!(player.playlist.current(), Some(0), "index shifted down");
        assert_eq!(load_paths(&cmds).len(), loads, "no engine traffic");

        player.handle(PlayerCommand::RemoveTrack(0)); // the current one
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(player.playlist.current(), None);
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Stop)));
    }

    #[test]
    fn clear_stops_and_keeps_playlist_commands_busy_on_an_empty_list() {
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::Play);
        player.handle(PlayerCommand::ClearPlaylist);
        assert_eq!(player.state, PlayState::Stopped);
        assert!(player.playlist.is_empty());
        // Transport on the empty list is a no-op, never a panic.
        player.handle(PlayerCommand::Next);
        player.handle(PlayerCommand::Play);
        player.handle(PlayerCommand::ToggleShuffle);
        assert_eq!(player.state, PlayState::Stopped);
        let loads = load_paths(&cmds).len();
        assert_eq!(loads, 1, "nothing was (re)started");
    }

    #[test]
    fn move_reorders_without_touching_playback() {
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::PlayTrack(1));
        let loads = load_paths(&cmds);
        player.handle(PlayerCommand::MoveTrack { from: 1, up: true });
        assert_eq!(player.state, PlayState::Playing);
        assert_eq!(load_paths(&cmds), loads, "no reload on reorder");
        assert_eq!(
            player.playlist.tracks()[0].title.as_deref(),
            Some("Track 1")
        );
    }

    #[test]
    fn volume_clamps_and_shuffle_repeat_toggle_with_notices() {
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::SetVolume(200));
        assert_eq!(player.snapshot().volume, 100);
        player.handle(PlayerCommand::SetVolume(42));
        assert_eq!(player.snapshot().volume, 42);

        player.handle(PlayerCommand::Play);
        let loads = load_paths(&cmds);
        player.handle(PlayerCommand::ToggleShuffle);
        assert!(player.snapshot().shuffle);
        assert_eq!(player.take_notice().as_deref(), Some("shuffle on"));
        player.handle(PlayerCommand::ToggleShuffle);
        assert!(!player.snapshot().shuffle);
        assert_eq!(player.take_notice().as_deref(), Some("shuffle off"));
        player.handle(PlayerCommand::ToggleRepeat);
        assert!(player.snapshot().repeat);
        assert_eq!(player.take_notice().as_deref(), Some("repeat on"));
        assert_eq!(
            load_paths(&cmds),
            loads,
            "toggling order/repeat never interrupts playback"
        );
        assert_eq!(player.state, PlayState::Playing);
    }

    #[test]
    fn shuffled_next_covers_the_whole_playlist_before_repeating() {
        let (mut player, _cmds, events) = fake_player();
        player.handle(PlayerCommand::AddTracks(tracks(6)));
        player.handle(PlayerCommand::ToggleShuffle);
        player.handle(PlayerCommand::Play);
        let mut visited = vec![player.playlist.current().expect("playing")];
        for _ in 0..6 {
            queue(&events, EngineEvent::Finished);
            player.tick(Duration::ZERO);
            visited.push(player.playlist.current().expect("playing"));
        }
        // First track once more at the end (round over, repeat off → stop).
        let mut unique = visited.clone();
        unique.truncate(6);
        unique.sort_unstable();
        assert_eq!(
            unique,
            (0..6).collect::<Vec<_>>(),
            "one full round: {visited:?}"
        );
        assert_eq!(player.state, PlayState::Stopped, "round over, repeat off");
    }

    // --- position math (pure) ---

    #[test]
    fn position_is_start_offset_plus_frames_over_rate() {
        assert_eq!(
            position(Duration::ZERO, 44_100, 44_100),
            Duration::from_secs(1)
        );
        assert_eq!(
            position(Duration::from_secs(10), 44_100, 44_100),
            Duration::from_secs(11)
        );
        // 96 kHz: 96 000 frames = 1 s exactly, half a second = 48 000.
        assert_eq!(
            position(Duration::ZERO, 96_000, 96_000),
            Duration::from_secs(1)
        );
        assert_eq!(
            position(Duration::from_millis(500), 48_000, 96_000),
            Duration::from_millis(1_000)
        );
        // A zero rate never divides by zero: offset only.
        assert_eq!(
            position(Duration::from_secs(3), 100, 0),
            Duration::from_secs(3)
        );
    }

    // --- spectrum smoothing ---

    #[test]
    fn paused_bars_decay_and_stopped_bars_zero() {
        let (mut player, _cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        // Inject a hot spectrum directly (the smoothing loop is what we test).
        player.spectrum.iter_mut().for_each(|b| *b = 1.0);
        player.peaks.iter_mut().for_each(|p| *p = 1.0);

        player.handle(PlayerCommand::Pause);
        player.tick(Duration::ZERO);
        let decayed = player.snapshot().spectrum[0];
        assert!(
            (decayed - 0.75).abs() < 1e-6,
            "paused decays ×0.75 per frame: {decayed}"
        );

        player.handle(PlayerCommand::Stop);
        player.tick(Duration::ZERO);
        let snapshot = player.snapshot();
        assert!(
            snapshot.spectrum.iter().all(|v| *v == 0.0),
            "stopped zeroes"
        );
        assert!(snapshot.peaks.iter().all(|v| *v == 0.0));
    }
}
