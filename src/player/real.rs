//! The real player: playlist + play order + the audio engine (plan 03).
//!
//! Talks to the engine through the [`AudioOut`] trait — production wields the
//! [`Engine`] thread, tests inject a fake that records commands and replays
//! events — so every state-machine rule here is testable without an audio
//! device. `position` and the spectrum stay zero until plan 04.

use std::time::Duration;

use crate::audio::{AudioOut, Engine, EngineCmd, EngineEvent, StreamInfo};

use super::order::PlayOrder;
use super::playlist::Playlist;
use super::rng::Rng;
use super::{FormatInfo, PlayState, PlayerCommand, PlayerSnapshot, TrackSource};

/// Spectrum/peak bars rendered while real playback reports no analyzer yet
/// (plan 04 replaces these with a real FFT).
const SPECTRUM_BINS: usize = 24;

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
            // plan 04: the timer starts counting.
            position: Duration::ZERO,
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
            // plan 04: real FFT bins; for now an all-zero spectrum.
            spectrum: vec![0.0; SPECTRUM_BINS],
            peaks: vec![0.0; SPECTRUM_BINS],
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
                    if self.playlist.current().is_none() {
                        // Nothing current: start at playlist track 1 and put
                        // the order's cursor on it (otherwise the first
                        // auto-advance would replay it).
                        self.order.jump_to(0);
                        self.start_track(0);
                    } else {
                        let current = self.playlist.current().expect("checked above");
                        self.start_track(current);
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
            // plan 04: restart the track when position > 3 s; until the clock
            // exists Prev always steps back (first track → restart).
            PlayerCommand::Prev => {
                if let Some(i) = self.order.prev() {
                    self.start_track(i);
                }
            }
            PlayerCommand::PlayTrack(i) if i < self.playlist.len() => {
                self.order.jump_to(i);
                self.start_track(i);
            }
            PlayerCommand::PlayTrack(_) => {}
            PlayerCommand::SeekBy(_) => { /* plan 04: seeking */ }
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

    /// Advance the clock: drain engine events into UI state.
    pub fn tick(&mut self, _dt: Duration) {
        for event in self.engine.poll() {
            match event {
                EngineEvent::Started(info) => self.on_started(info),
                EngineEvent::Finished => self.advance_after_end(),
                EngineEvent::Error(reason) => self.on_error(reason),
            }
        }
    }

    /// The track opened: hi-fi line from the stream, streak reset.
    fn on_started(&mut self, info: StreamInfo) {
        self.fail_streak = 0;
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
    /// once the failures span the whole playlist.
    fn on_error(&mut self, reason: String) {
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

    /// Load a track into the engine and mark it current + playing.
    fn start_track(&mut self, i: usize) {
        let Some(TrackSource::Local(path)) = self
            .playlist
            .tracks()
            .get(i)
            .map(|track| track.source.clone())
        else {
            return;
        };
        self.playlist.set_current(Some(i));
        self.state = PlayState::Playing;
        self.engine.send(EngineCmd::Load { path });
    }

    /// Halt playback but keep the current track selected (Stop semantics).
    fn stop_playback(&mut self) {
        self.engine.send(EngineCmd::Stop);
        self.state = PlayState::Stopped;
        self.format = FormatInfo::blank();
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
    use std::sync::{Arc, Mutex};

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

    fn load_paths(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<PathBuf> {
        log_of(cmds)
            .into_iter()
            .filter_map(|cmd| match cmd {
                EngineCmd::Load { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    fn queue(events: &Arc<Mutex<Vec<EngineEvent>>>, event: EngineEvent) {
        events.lock().expect("events lock").push(event);
    }

    /// A 3-track list, no track playing yet (Add does not autoplay).
    fn three_tracks() -> Harness {
        let (mut player, cmds, events) = fake_player();
        player.handle(PlayerCommand::AddTracks(tracks(3)));
        assert!(load_paths(&cmds).is_empty(), "Add must not autoplay");
        (player, cmds, events)
    }

    fn fake_started() -> StreamInfo {
        StreamInfo {
            codec: "FLAC".into(),
            lossless: true,
            sample_rate: 44_100,
            channels: 2,
            bit_depth: Some(16),
            duration: Some(Duration::from_secs(1)),
        }
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
    fn prev_always_steps_back_for_now() {
        // plan 04: restart when position > 3 s. Until then Prev always moves.
        let (mut player, cmds, _events) = three_tracks();
        player.handle(PlayerCommand::PlayTrack(1));
        player.handle(PlayerCommand::Prev);
        assert_eq!(player.playlist.current(), Some(0));
        // First track → restart it (same track again).
        player.handle(PlayerCommand::Prev);
        assert_eq!(player.playlist.current(), Some(0));
        assert_eq!(load_paths(&cmds).len(), 3, "restart issues another Load");
    }

    #[test]
    fn seek_is_ignored_until_plan_04() {
        let (mut player, _cmds, _events) = three_tracks();
        player.handle(PlayerCommand::Play);
        let before = player.snapshot().position;
        player.handle(PlayerCommand::SeekBy(30));
        player.tick(Duration::from_secs(30));
        assert_eq!(player.snapshot().position, before);
        assert_eq!(before, Duration::ZERO, "position reads 0:00 (plan 04)");
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
        queue(&events, EngineEvent::Started(fake_started()));
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
        queue(&events, EngineEvent::Started(lossy));
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
    fn snapshot_reports_zero_time_and_an_empty_spectrum() {
        let (mut player, _cmds, events) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, EngineEvent::Started(fake_started()));
        player.tick(Duration::from_secs(5));
        let snapshot = player.snapshot();
        assert_eq!(snapshot.position, Duration::ZERO, "plan 04 owns the clock");
        assert_eq!(snapshot.spectrum.len(), 24);
        assert!(snapshot.spectrum.iter().all(|v| *v == 0.0));
        assert!(snapshot.peaks.iter().all(|v| *v == 0.0));
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
}
