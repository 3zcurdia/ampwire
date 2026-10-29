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

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::audio::{
    Analyzer, AudioOut, BANDS, Engine, EngineCmd, EngineEvent, NO_BOUNDARY, PlayError,
    PlaybackHandle, QueueSource, SharedParams, StreamInfo, StreamSetup, balance_to_gains,
    volume_to_gain,
};
use crate::config::{JellyfinConfig, JellyfinMode, SmbConfig, SmbMode};
use crate::jellyfin::cache::{
    Cache, CacheEvent, DIR_PREFIX as JELLYFIN_DIR_PREFIX, STALE_AGE, compute_window,
    sweep_stale_dirs,
};
use crate::jellyfin::{Client, ItemId};
use crate::smb::SmbUrl;
use crate::smb::cache::{
    DIR_PREFIX as SMB_DIR_PREFIX, SmbCache, SmbCacheEvent, compute_smb_window,
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
    /// Volume 0–=100 (default 100 = unity). Applied to the samples by the
    /// callback's `Dsp` through the shared targets.
    volume: u8,
    /// Balance −100..=100 (default 0). Same path as `volume`.
    balance: i8,
    /// L/R swap for reversed-wiring devices (`[audio] swap_channels`).
    swap_channels: bool,
    /// EQ on *and* not flat (plan 06): any non-zero preamp/band shapes the
    /// samples, so the bit-perfect marker must go.
    eq_active: bool,
    /// The current source survives the `f32` pipeline exactly
    /// ([`StreamInfo::f32_exact`]); false rules out the `bit-perfect` marker.
    source_exact: bool,
    /// Volume/balance/EQ targets shared with the audio callback (lock-free).
    params: Arc<SharedParams>,
    /// From [`EngineEvent::Started`]; blank while stopped.
    format: FormatInfo,
    /// Live clock + sample tap from the engine; `None` while stopped.
    handle: Option<PlaybackHandle>,
    /// The next track's info from [`EngineEvent::Advanced`], waiting for the
    /// audio to reach `PlaybackHandle::boundary` (plan 09, step 4): the
    /// event can precede the audio by the ring's length, so the cursor
    /// moves only when the clock really crosses.
    pending_advance: Option<StreamInfo>,
    /// Frames-played value the current track started at (plan 09): `0` for
    /// a loaded track (its decoder's `start_offset` carries the seek), the
    /// boundary for a gapless-advanced one.
    track_base: u64,
    /// Where the current track's own clock starts (plan 09): the decoder's
    /// start offset for a loaded track, `ZERO` after a gapless advance.
    track_start_offset: Duration,
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
    /// How remote tracks are fetched (plan 08); `None` when `[jellyfin]`
    /// is absent — no remote track can then ever reach this player.
    jf_mode: Option<JellyfinMode>,
    /// Rolling-window size in remote tracks (`[jellyfin] cache_ahead/behind`).
    jf_ahead: usize,
    jf_behind: usize,
    /// The rolling temp-dir cache (`mode = "cache"`); `None` in the stream
    /// modes (and in tests).
    cache: Option<Cache>,
    /// A remote track is waiting for its file to finish downloading (cache
    /// mode). Cleared as soon as the download resolves or the user moves on.
    pending_remote: Option<ItemId>,
    /// `[smb]` for this session (plan 10, step 5): credentials, mode and
    /// window size. `None` → guest credentials, `cache` mode, 3/3 — the
    /// plan's defaults, so a guest-only share needs no config at all.
    smb: Option<SmbConfig>,
    /// The rolling SMB window (`mode = "cache"`), created on the *first*
    /// SMB track: a session that never touches SMB never opens a directory.
    smb_cache: Option<SmbCache>,
    /// An SMB track is waiting for its file to finish downloading (cache
    /// mode) — the mirror of `pending_remote`.
    pending_smb: Option<SmbUrl>,
    /// `stream pure: eq bypassed` was already shown this session (plan 08:
    /// notice once, not on every track change).
    jf_pure_notice_shown: bool,
}

impl RealPlayer {
    /// The production player: an engine thread that owns the audio device.
    /// The shared DSP targets are created here and cloned (`Arc`) into the
    /// engine's callback — one instance, written by this thread, read by the
    /// audio thread.
    ///
    /// `jellyfin` is the session's `[jellyfin]` config and `smb` its
    /// `[smb]` config: crash leftovers (`ampwire-jellyfin-<pid>` and
    /// `ampwire-smb-<pid>`, older than 24 h) are swept at startup in every
    /// mode; the Jellyfin `cache` mode also creates its temp-dir window
    /// eagerly (the stream modes create one only if a server forces the
    /// no-`Range` fallback), while the SMB window is created on the first
    /// SMB track.
    pub fn new(jellyfin: Option<&JellyfinConfig>, smb: Option<&SmbConfig>) -> Self {
        let params = Arc::new(SharedParams::new());
        let config = jellyfin.filter(|config| config.is_complete());
        if let Some(config) = config {
            let parent = config.cache_dir.clone().unwrap_or_else(std::env::temp_dir);
            sweep_stale_dirs(&parent, STALE_AGE, JELLYFIN_DIR_PREFIX);
        }
        // Plan 10, step 4/5: same rule for the SMB window's directory, in
        // every mode — `cache_dir` (or the system temp dir) is where it
        // would have been created.
        let smb_parent = smb
            .and_then(|config| config.cache_dir.clone())
            .unwrap_or_else(std::env::temp_dir);
        sweep_stale_dirs(&smb_parent, crate::smb::cache::STALE_AGE, SMB_DIR_PREFIX);
        // The engine holds the client for `stream*` modes (it builds the
        // HTTP source itself, so credentials never reach a `Track`) and
        // doubles its ring buffer when streaming (plan 08, step 6).
        let stream = config
            .filter(|config| config.mode != JellyfinMode::Cache)
            .map(|config| StreamSetup {
                client: Client::new(config),
                fallback_parent: config.cache_dir.clone(),
            });
        // Plan 10, step 6: in `[smb] mode = "stream"` the *engine* holds the
        // credentials (it builds each seekable source itself, so they never
        // reach a `Track`) and doubles its ring buffer — the same rule as
        // the Jellyfin `StreamSetup` above. Cache mode needs no setup: the
        // engine only ever receives a local path.
        let smb_setup = smb
            .filter(|config| config.mode == SmbMode::Stream)
            .map(|config| crate::smb::SmbSetup {
                cfg: crate::smb::SmbClientCfg::credentials(Some(config)),
            });
        let engine = Box::new(Engine::spawn(Arc::clone(&params), stream, smb_setup));
        let mut player = Self::with_engine(engine, params);
        if let Some(config) = config {
            player.jf_mode = Some(config.mode);
            player.jf_ahead = config.cache_ahead;
            player.jf_behind = config.cache_behind;
            if config.mode == JellyfinMode::Cache {
                match Cache::new(Client::new(config), config.cache_dir.clone()) {
                    Ok(cache) => {
                        player.cache = Some(cache);
                    }
                    Err(reason) => {
                        player.notice = Some(format!("jellyfin: cache unavailable ({reason})"))
                    }
                }
            }
        }
        player.smb = smb.cloned();
        player
    }

    /// Same state machine over a different [`AudioOut`] (tests). The tests
    /// hold their own `Arc` clone of `params` to inspect what was written.
    fn with_engine(engine: Box<dyn AudioOut>, params: Arc<SharedParams>) -> Self {
        Self {
            playlist: Playlist::new(),
            state: PlayState::Stopped,
            shuffle: false,
            repeat: false,
            // Full volume, centered balance, no swap: unity targets, so the
            // callback's DSP lets samples pass through untouched
            // (plans-README rule 7).
            volume: 100,
            balance: 0,
            swap_channels: false,
            eq_active: false,
            source_exact: true,
            params,
            format: FormatInfo::blank(),
            handle: None,
            pending_advance: None,
            track_base: 0,
            track_start_offset: Duration::ZERO,
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
            jf_mode: None,
            jf_ahead: 3,
            jf_behind: 3,
            cache: None,
            pending_remote: None,
            smb: None,
            smb_cache: None,
            pending_smb: None,
            jf_pure_notice_shown: false,
        }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let format = if self.state == PlayState::Stopped {
            FormatInfo::blank()
        } else {
            let mut format = self.format.clone();
            // Bit-perfect marker (plan 05/06): only a lossless source with
            // no DSP touching the samples — full volume, centered balance,
            // no channel swap, and an EQ that is off, flat or bypassed for
            // this track (`stream_pure`, plan 08 — same condition the DSP
            // uses). A swap reorders the stream — sample *values* survive,
            // but the device no longer receives the decoded layout — so it
            // counts as processing too.
            format.bit_perfect = format.lossless
                && self.source_exact
                && self.volume == 100
                && self.balance == 0
                && !self.swap_channels
                && !self.eq_shaping();
            format
        };
        PlayerSnapshot {
            state: self.state,
            current: self.playlist.current(),
            position: self.position(),
            tracks: self.playlist.tracks().clone(),
            shuffle: self.shuffle,
            repeat: self.repeat,
            volume: self.volume,
            balance: self.balance,
            format,
            spectrum: self.spectrum.clone(),
            peaks: self.peaks.clone(),
        }
    }

    /// Where the music is right now: the seek offset plus the frames the
    /// callback really delivered *since this track started* — after a
    /// gapless advance (plan 09) that is measured from the boundary, and
    /// the new track counts from 0 with no offset. Clamped to the duration
    /// when known. While a `Load` is still in flight (no handle yet), the
    /// pending start offset stands in, so seeks jump the clock instantly.
    fn position(&self) -> Duration {
        let position = match (&self.handle, self.pending_start) {
            (Some(handle), _) => {
                let frames = handle
                    .played
                    .load(Ordering::Relaxed)
                    .saturating_sub(self.track_base);
                self.track_start_offset + position(Duration::ZERO, frames, handle.sample_rate)
            }
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
            // Edits keep a running shuffle's order (audit P3): `appended`,
            // `removed` and `swapped` patch the permutation instead of
            // reshuffling it; replace/clear/toggle still rebuild.
            PlayerCommand::AddTracks(new) => {
                self.playlist.append_sorted(new);
                self.order
                    .appended(self.playlist.len(), self.playlist.current(), &mut self.rng);
                self.update_cache_window();
                self.refresh_queue();
                return;
            }
            PlayerCommand::AppendInOrder(new) => {
                self.playlist.append_in_order(new);
                self.order
                    .appended(self.playlist.len(), self.playlist.current(), &mut self.rng);
                self.update_cache_window();
                self.refresh_queue();
                return;
            }
            PlayerCommand::RemoveRemote => {
                // Reload rule (plan 08): drop the old *Jellyfin* batch;
                // stop if one of them was playing (mirrors `RemoveTrack`).
                // SMB tracks are remote too but stay: they were not loaded
                // by `j` (plan 10, step 2).
                let removed: Vec<bool> = self
                    .playlist
                    .tracks()
                    .iter()
                    .map(|track| track.is_jellyfin())
                    .collect();
                let was_current = self.playlist.remove_where(|track| track.is_jellyfin());
                self.order
                    .removed(&removed, self.playlist.current(), &mut self.rng);
                self.update_cache_window();
                self.refresh_queue();
                if was_current {
                    self.stop_playback();
                }
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
                let mut removed = vec![false; self.playlist.len()];
                if let Some(slot) = removed.get_mut(i) {
                    *slot = true;
                }
                let was_current = self.playlist.remove(i);
                self.order
                    .removed(&removed, self.playlist.current(), &mut self.rng);
                self.update_cache_window();
                self.refresh_queue();
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
                let to = if up {
                    self.playlist.move_up(from)
                } else {
                    self.playlist.move_down(from)
                };
                if let Some(to) = to {
                    self.order
                        .swapped(from, to, self.playlist.current(), &mut self.rng);
                    self.update_cache_window();
                    self.refresh_queue();
                }
                return;
            }
            // Volume/balance/swap/EQ work even with an empty playlist (they
            // are applied at startup from the config, before any track
            // loads).
            PlayerCommand::SetVolume(v) => self.set_volume(v),
            PlayerCommand::SetBalance(b) => self.set_balance(b),
            PlayerCommand::SetSwapChannels(swap) => self.set_swap_channels(swap),
            PlayerCommand::SetEq { enabled, gains } => self.set_eq(enabled, gains),
            _ => {}
        }
        self.handle_transport(cmd);
    }

    /// Volume 0–=100 → the shared gain target (0 = silence, 100 = unity).
    fn set_volume(&mut self, v: u8) {
        self.volume = v.min(100);
        let (left, right) = balance_to_gains(self.balance);
        self.params.set(volume_to_gain(self.volume), left, right);
    }

    /// Balance −100..=100 → the shared per-channel targets.
    fn set_balance(&mut self, b: i8) {
        self.balance = b.clamp(-100, 100);
        let (left, right) = balance_to_gains(self.balance);
        self.params.set(volume_to_gain(self.volume), left, right);
    }

    /// L/R swap for reversed-wiring devices: forwarded to the callback,
    /// which reorders every stereo pair after the gain stage.
    fn set_swap_channels(&mut self, swap: bool) {
        self.swap_channels = swap;
        self.params.set_swap_channels(swap);
    }

    /// EQ on/off + gains → the shared targets (plan 06). The callback picks
    /// them up through the version bump; `eq_active` mirrors what actually
    /// shapes the sound (enabled *and* not flat) for the bit-perfect marker.
    fn set_eq(&mut self, enabled: bool, gains: [i8; 11]) {
        let gains = gains.map(|g| g.clamp(-12, 12));
        self.eq_active = enabled && gains.iter().any(|g| *g != 0);
        self.params.set_eq(enabled, gains);
    }

    /// Whether `stream_pure` has the EQ bypassed *right now*: the mode is
    /// `stream_pure` and the current track is a Jellyfin item (an SMB track
    /// is remote but never bypassed — plan 10, step 2).
    fn eq_bypassed(&self) -> bool {
        matches!(self.jf_mode, Some(JellyfinMode::StreamPure))
            && self
                .playlist
                .current()
                .and_then(|index| self.playlist.tracks().get(index))
                .is_some_and(|track| track.is_jellyfin())
    }

    /// The EQ really shaping the samples right now — what the bit-perfect
    /// marker must account for: on, not flat, and not bypassed.
    fn eq_shaping(&self) -> bool {
        self.eq_active && !self.eq_bypassed()
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
            PlayerCommand::SeekTo(target) => self.seek_to(target),
            PlayerCommand::ToggleShuffle => {
                self.shuffle = !self.shuffle;
                self.rebuild_order();
                // The order changed: the queued next track must follow it
                // (plan 09, step 5).
                self.refresh_queue();
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
                // The plan lists repeat toggles among the window refresh
                // triggers; the wanted set cannot change, but recomputing
                // it is one cheap vector.
                self.update_cache_window();
                // Re-send the queue on every repeat toggle too (plan 09).
                self.refresh_queue();
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
        let target = (self.position().as_secs_f64() + delta as f64).max(0.0);
        self.seek_to(Duration::from_secs_f64(target));
    }

    /// Absolute seek (MPRIS `SetPosition` / `ampwire --ctl seek-to`). The
    /// same one-second end clamp as `seek_by`; pausing is preserved.
    fn seek_to(&mut self, target: Duration) {
        if !matches!(self.state, PlayState::Playing | PlayState::Paused) {
            return; // stopped: nothing to seek
        }
        let Some(i) = self.playlist.current() else {
            return;
        };
        let mut target = target.as_secs_f64().max(0.0);
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
                // Plan 09, step 4: remember the switch; the track change
                // itself happens when the clock reaches the boundary.
                EngineEvent::Advanced(info) => self.pending_advance = Some(info),
                EngineEvent::Finished => self.advance_after_end(),
                EngineEvent::Error(reason) => self.on_error(reason),
                // Plan 08, step 6: the engine announced the (blocking) probe
                // of a remote track, or the stream range-fallback notice.
                EngineEvent::Buffering => self.notice = Some("buffering…".into()),
                EngineEvent::Notice(text) => self.notice = Some(text),
            }
        }
        self.check_boundary();
        self.poll_cache();
        self.poll_smb_cache();
        self.decay_spectrum();
    }

    /// Plan 09, step 4: move `current` (cursor, clock base, format line)
    /// exactly when the audio crosses the engine's boundary — never when
    /// [`EngineEvent::Advanced`] arrives, which can precede the audio by
    /// the ring's length. The stream, the handle and the tap carry on
    /// untouched; only this side changes track.
    fn check_boundary(&mut self) {
        let Some(info) = self.pending_advance.take() else {
            return;
        };
        let Some(handle) = &self.handle else {
            return; // stopped before the boundary: nothing to advance to
        };
        let boundary = handle.boundary.load(Ordering::Acquire);
        // `NO_BOUNDARY` = the engine never recorded one (defensive: an
        // `Advanced` always precedes its store, but a handle without a
        // boundary can never be crossed).
        if boundary == NO_BOUNDARY || handle.played.load(Ordering::Acquire) < boundary {
            self.pending_advance = Some(info); // not heard yet: wait a tick
            return;
        }
        // The audio really crossed into the queued track (the engine had
        // queued `order.upcoming(1)`, so `order.next` returns exactly it —
        // repeat wraps and end-of-order were never queued).
        self.track_base = boundary;
        self.track_start_offset = Duration::ZERO;
        self.pending_start = None;
        self.fail_streak = 0;
        match self.order.next(self.repeat, &mut self.rng) {
            Some(i) => {
                self.playlist.set_current(Some(i));
                self.apply_format(&info, Some(i));
                // `stream_pure` (plan 08): same once-per-session notice as
                // at `Started`, for the case that starts locally.
                if self.eq_bypassed() && !self.jf_pure_notice_shown {
                    self.jf_pure_notice_shown = true;
                    self.notice = Some("stream pure: eq bypassed".into());
                }
                self.update_cache_window();
                // The next-next track becomes the queued one (plan 09).
                self.refresh_queue();
            }
            // The playlist emptied under the queue (an edit raced the
            // audio): fall back to the normal end-of-track stop.
            None => self.stop_playback(),
        }
    }

    /// The track opened: clock + tap handle, hi-fi line, streak reset, the
    /// decoder's accurate duration when the tag had none or lied (the tag
    /// is off by more than a second) — and the gapless queue for what
    /// plays next (plan 09, step 5).
    fn on_started(&mut self, info: StreamInfo, handle: PlaybackHandle) {
        self.fail_streak = 0;
        // Buffering is over: retire that exact notice if it is still up
        // (anything else queued meanwhile keeps its place).
        if self.notice.as_deref() == Some("buffering…") {
            self.notice = None;
        }
        // The fresh stream's clock starts at its own offset (a seeked-to
        // position), and supersedes any in-flight gapless advance: that
        // boundary died with the stream the engine just dropped.
        self.track_start_offset = handle.start_offset;
        self.track_base = 0;
        self.pending_advance = None;
        let current = self.playlist.current();
        self.handle = Some(handle);
        self.pending_start = None; // the live clock takes over
        self.apply_format(&info, current);
        // `stream_pure` (plan 08): first remote track of the session — say
        // what is different about it. Shown at `Started`, so it is never
        // clobbered by (and never hides) the `buffering…` it follows.
        if self.eq_bypassed() && !self.jf_pure_notice_shown {
            self.jf_pure_notice_shown = true;
            self.notice = Some("stream pure: eq bypassed".into());
        }
        self.refresh_queue();
    }

    /// The format line — and the playlist's duration fix — for track `i`
    /// from the decoder's [`StreamInfo`]. Shared by [`Self::on_started`]
    /// and the gapless [`EngineEvent::Advanced`] crossing (plan 09), which
    /// swaps the current track without touching the live stream.
    fn apply_format(&mut self, info: &StreamInfo, i: Option<usize>) {
        if let Some(decoded) = info.duration {
            let tagged = i
                .and_then(|i| self.playlist.tracks().get(i))
                .and_then(|t| t.duration);
            let tag_lies = tagged
                .is_none_or(|tagged| (tagged.as_secs_f64() - decoded.as_secs_f64()).abs() > 1.0);
            if tag_lies && let Some(i) = i {
                self.playlist.set_duration(i, decoded);
            }
        }
        // Codec / depth / rate / channels / lossless come from the decoder's
        // StreamInfo; the bitrate (lossy only) from the scanned tag.
        let bitrate_kbps = if info.lossless {
            None
        } else {
            i.and_then(|i| self.playlist.tracks().get(i))
                .and_then(|t| t.bitrate_kbps)
        };
        self.source_exact = info.f32_exact;
        self.format = FormatInfo {
            codec: info.codec.clone(),
            // The marker itself is computed in `snapshot()` from the live
            // volume/balance (plan 05); the stored format only carries the
            // source's losslessness.
            bit_perfect: false,
            bit_depth: info.bit_depth,
            sample_rate: Some(info.sample_rate),
            channels: channels_label(info.channels),
            bitrate_kbps,
            lossless: info.lossless,
        };
    }

    /// A track could not be played: notice + skip to the next one, or stop
    /// once the failures span the whole playlist. A failed *seek* is not a
    /// bad file: restart the same track from 0 with a notice instead.
    fn on_error(&mut self, error: PlayError) {
        let reason = match error {
            PlayError::SeekUnsupported => {
                self.notice = Some("seek not supported for this file".into());
                self.reload_current();
                return;
            }
            PlayError::Failed(reason) => reason,
        };
        self.fail_streak += 1;
        let name = self
            .playlist
            .current()
            .and_then(|i| self.playlist.tracks().get(i))
            .map(|t| t.display_name())
            .unwrap_or_else(|| "track".into());
        if self.fail_streak >= self.playlist.len() {
            // Keep the last reason in the give-up notice: without it the
            // user only sees silence and "no playable tracks" — e.g. a
            // headset jack that cannot hold the file's rate is invisible.
            self.stop_playback();
            self.notice = Some(format!("no playable tracks: {reason}"));
        } else {
            self.notice = Some(format!("skipped {name}: {reason}"));
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
    ///
    /// Remote tracks go through [`RealPlayer::start_remote`] instead: the
    /// cache plays a local file (ready) or buffers (not ready yet), and the
    /// stream modes ask the engine for an HTTP `Range` source.
    fn load_track(&mut self, i: usize, start: Duration, keep_paused: bool) {
        let Some(source) = self
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
        // offset stands in until the engine reports `Started` (or, while a
        // remote file buffers, until the cache reports `Ready`).
        self.handle = None;
        self.pending_start = Some(start);
        self.pending_remote = None;
        self.pending_smb = None;
        // `stream_pure` (plan §6): while a *Jellyfin* remote track plays,
        // the EQ is bypassed on the audio side; the very next load restores
        // it. SMB tracks are remote too, but never bypassed (plan 10).
        let bypass = matches!(self.jf_mode, Some(JellyfinMode::StreamPure))
            && self
                .playlist
                .tracks()
                .get(i)
                .is_some_and(|track| track.is_jellyfin());
        self.params.set_eq_bypass(bypass);
        let engine_load = match source {
            TrackSource::Local(path) => {
                self.engine.send(EngineCmd::Load { path, start });
                true
            }
            TrackSource::Remote { item_id, container } => {
                self.start_remote(&item_id, container.as_deref(), start, keep_paused)
            }
            TrackSource::Smb { url } => self.start_smb(&url, start, keep_paused),
        };
        if engine_load {
            if keep_paused {
                self.engine.send(EngineCmd::Pause);
            } else {
                self.engine.send(EngineCmd::Resume);
            }
        }
        // Every track change reshuffles the rolling window around the new
        // current track (plan §5).
        self.update_cache_window();
    }

    /// Start a remote (Jellyfin) track (plan 08, step 5). Cache mode: play
    /// the local file when it is ready; otherwise show `buffering…`, keep
    /// `state = Playing` and remember the item until its `CacheEvent`
    /// arrives. Returns true when a `Load` went to the engine, so the
    /// caller still re-asserts the pause state around it.
    fn start_remote(
        &mut self,
        item_id: &ItemId,
        container: Option<&str>,
        start: Duration,
        keep_paused: bool,
    ) -> bool {
        match self.jf_mode.unwrap_or(JellyfinMode::Cache) {
            JellyfinMode::Cache => {
                let Some(cache) = &self.cache else {
                    // No window to download into (the cache dir could not be
                    // created): stop with the reason instead of skipping
                    // through every remote track in the list.
                    self.stop_playback();
                    self.notice = Some("jellyfin: cache unavailable".into());
                    return false;
                };
                if let Some(path) = cache.path_if_ready(item_id) {
                    // A local file under our own name: seek, EQ and the
                    // spectrum tap all work unchanged.
                    self.engine.send(EngineCmd::Load { path, start });
                    return true;
                }
                self.notice = Some("buffering…".into());
                self.pending_remote = Some(item_id.clone());
                let _ = keep_paused; // the state was already set by load_track
                false
            }
            // Stream modes (plan 08, step 6): the engine opens the seekable
            // HTTP source itself — and announces `buffering…` before the
            // probe blocks it for a round trip.
            JellyfinMode::Stream | JellyfinMode::StreamPure => {
                self.engine.send(EngineCmd::LoadRemote {
                    item_id: item_id.clone(),
                    container: container.map(str::to_string),
                    start,
                });
                true
            }
        }
    }

    /// Start an SMB track (plan 10, step 5): cache mode plays the local file
    /// once the rolling window has downloaded it (else `buffering…`, keep
    /// `state = Playing` and remember the URL until its [`SmbCacheEvent`]
    /// arrives) — exactly the shape of `start_remote`. Stream mode hands the
    /// engine an `EngineCmd::LoadSmb` (step 6). Returns true when a `Load`
    /// went to the engine, so the caller still re-asserts the pause state
    /// around it.
    fn start_smb(&mut self, url: &SmbUrl, start: Duration, keep_paused: bool) -> bool {
        match self.smb_mode() {
            SmbMode::Cache => {
                // Created here rather than in `new()`: a session that never
                // touches SMB never opens a cache directory — and a
                // guest-only share needs no `[smb]` section to have one.
                if self.smb_cache.is_none() {
                    let parent = self
                        .smb
                        .as_ref()
                        .and_then(|config| config.cache_dir.clone());
                    let cfg = crate::smb::SmbClientCfg::for_url(url, self.smb.as_ref());
                    match SmbCache::new(cfg, parent) {
                        Ok(cache) => self.smb_cache = Some(cache),
                        Err(_) => {
                            // No window to download into: stop with the
                            // reason instead of skipping through every SMB
                            // track in the list (the plan's cache rule).
                            self.stop_playback();
                            self.notice = Some("smb: cache unavailable".into());
                            return false;
                        }
                    }
                }
                let ready = self
                    .smb_cache
                    .as_ref()
                    .and_then(|cache| cache.path_if_ready(url));
                if let Some(path) = ready {
                    // A local file under our own name: seek, EQ and the
                    // spectrum tap all work unchanged.
                    self.engine.send(EngineCmd::Load { path, start });
                    return true;
                }
                self.notice = Some("buffering…".into());
                self.pending_smb = Some(url.clone());
                let _ = keep_paused; // the state was already set by load_track
                false
            }
            // Stream mode (plan 10, step 6): the engine opens the seekable
            // source itself — connect, probe and the `buffering…` announce
            // all happen on the engine thread, and the credentials live in
            // its `SmbSetup`, never in a `Track`. `start` carries the seek,
            // which is a reload at the offset (exactly like `LoadRemote`).
            SmbMode::Stream => {
                self.engine.send(EngineCmd::LoadSmb {
                    url: url.clone(),
                    start,
                });
                true
            }
        }
    }

    /// The session's SMB mode: `cache` when there is no `[smb]` section
    /// (the plan's default).
    fn smb_mode(&self) -> SmbMode {
        self.smb
            .as_ref()
            .map_or(SmbMode::Cache, |config| config.mode)
    }

    /// The rolling window's size (plan 10: `[smb] cache_ahead/behind`,
    /// defaulting to the plan's 3/3).
    fn smb_window(&self) -> (usize, usize) {
        self.smb
            .as_ref()
            .map_or((3, 3), |config| (config.cache_ahead, config.cache_behind))
    }

    /// Keep the engine's gapless queue in step with the play order (plan
    /// 09, step 5): the next track of `PlayOrder::upcoming(1)` is
    /// pre-opened right after every `Started` and every boundary crossing,
    /// and re-sent (or dropped) whenever the order changes — shuffle/repeat
    /// toggle, playlist edit, `PlayTrack`. End of order and a stopped
    /// player queue nothing (`Unqueue`); repeat *wraps* are never queued
    /// either, because `upcoming` never wraps.
    fn refresh_queue(&mut self) {
        let queued = if self.state == PlayState::Stopped {
            None
        } else {
            self.order
                .upcoming(1)
                .next()
                .and_then(|i| self.queue_source(i))
        };
        match queued {
            Some((source, eq_bypass)) => {
                self.engine.send(EngineCmd::Queue {
                    source,
                    start: Duration::ZERO,
                    eq_bypass,
                });
            }
            None => self.engine.send(EngineCmd::Unqueue),
        }
    }

    /// What the engine should pre-open for playlist track `i` (plan 09),
    /// as `(source, stream_pure bypass)`: a local path — a cache-mode
    /// remote track only once its file has been downloaded — or a
    /// `stream*`-mode HTTP source.
    fn queue_source(&self, i: usize) -> Option<(QueueSource, bool)> {
        let track = self.playlist.tracks().get(i)?;
        let source = match &track.source {
            TrackSource::Local(path) => QueueSource::Local(path.clone()),
            TrackSource::Remote { item_id, container } => match self.jf_mode {
                // Cache mode plays remote tracks from disk: only a file
                // that already exists can be pre-opened (still downloading
                // → no queue; the normal buffering path takes over).
                Some(JellyfinMode::Cache) | None => {
                    QueueSource::Local(self.cache.as_ref()?.path_if_ready(item_id)?)
                }
                Some(JellyfinMode::Stream | JellyfinMode::StreamPure) => QueueSource::Remote {
                    item_id: item_id.clone(),
                    container: container.clone(),
                },
            },
            // Plan 10, step 7: cache mode queues only a file the window has
            // *already* downloaded (still downloading → no queue; the normal
            // buffering path takes over), stream mode is always queueable —
            // SMB reads are positioned, so there is no "server without
            // ranges" case to fall back from.
            TrackSource::Smb { url } => match self.smb_mode() {
                SmbMode::Cache => QueueSource::Local(self.smb_cache.as_ref()?.path_if_ready(url)?),
                SmbMode::Stream => QueueSource::Smb(url.clone()),
            },
        };
        let eq_bypass =
            matches!(self.jf_mode, Some(JellyfinMode::StreamPure)) && track.is_jellyfin();
        Some((source, eq_bypass))
    }

    /// Push the current rolling window to the cache downloader: the current
    /// remote track → the next `jf_ahead` → the previous `jf_behind`, in
    /// *play* order (so shuffle is respected). Called after every track
    /// change, playlist mutation and order/repeat toggle — which is also
    /// what "a library page batch finished" becomes, since each page lands
    /// as an `AppendInOrder` mutation.
    fn update_cache_window(&self) {
        // Plan 10, step 5: the SMB rolling window moves on exactly the same
        // events (track change, playlist edit, order toggle) as the
        // Jellyfin one — one call site, two windows.
        self.update_smb_window();
        if self.cache.is_none() {
            return;
        }
        let wanted = compute_window(
            &self.order,
            self.playlist.tracks(),
            self.playlist.current(),
            self.jf_ahead,
            self.jf_behind,
        );
        if let Some(cache) = &self.cache {
            cache.set_window(wanted);
        }
    }

    /// Push the current rolling window to the SMB downloader: the current
    /// SMB track → the next `cache_ahead` → the previous `cache_behind`, in
    /// *play* order (so shuffle is respected). A no-op until the first SMB
    /// track creates the window.
    fn update_smb_window(&self) {
        let Some(cache) = &self.smb_cache else {
            return;
        };
        let (ahead, behind) = self.smb_window();
        let wanted = compute_smb_window(
            &self.order,
            self.playlist.tracks(),
            self.playlist.current(),
            ahead,
            behind,
        );
        cache.set_window(wanted);
    }

    /// Drain downloader events (plan §5): the `Ready` of the track we are
    /// waiting on becomes the engine `Load`, a `Failed` takes the plan-03
    /// error/skip path, and anything else is a prefetch result the cache
    /// already recorded — ignored here.
    fn poll_cache(&mut self) {
        let events = match self.cache.as_mut() {
            Some(cache) => cache.poll(),
            None => return,
        };
        for event in events {
            self.handle_cache_event(event);
        }
    }

    /// Apply one downloader event (split out of [`RealPlayer::poll_cache`]
    /// so the state machine is testable without a server).
    fn handle_cache_event(&mut self, event: CacheEvent) {
        match event {
            CacheEvent::Ready { item_id, path } => {
                if self.pending_remote.as_ref() != Some(&item_id) {
                    return; // stale: the user already moved on
                }
                self.pending_remote = None;
                self.load_ready_file(path);
            }
            CacheEvent::Failed { item_id, error } => {
                if self.pending_remote.as_ref() != Some(&item_id) {
                    return;
                }
                self.pending_remote = None;
                // Plan-03 path: "skipped …: reason", or give up when the
                // failures span the playlist.
                self.on_error(PlayError::Failed(error));
            }
        }
    }

    /// Drain the SMB downloader's events (plan 10, step 5): the `Ready` of
    /// the track we are waiting on becomes the engine `Load`, a `Failed`
    /// takes the plan-03 error/skip path, and anything else is a prefetch
    /// result the cache already recorded — ignored here.
    fn poll_smb_cache(&mut self) {
        let events = match self.smb_cache.as_mut() {
            Some(cache) => cache.poll(),
            None => return,
        };
        for event in events {
            self.handle_smb_cache_event(event);
        }
    }

    /// Apply one SMB downloader event (split out of `poll_smb_cache` so the
    /// state machine is testable without a server).
    fn handle_smb_cache_event(&mut self, event: SmbCacheEvent) {
        match event {
            SmbCacheEvent::Ready { url, path } => {
                if self.pending_smb.as_ref() != Some(&url) {
                    return; // stale: the user already moved on
                }
                self.pending_smb = None;
                self.load_ready_file(path);
            }
            SmbCacheEvent::Failed { url, error } => {
                if self.pending_smb.as_ref() != Some(&url) {
                    return;
                }
                self.pending_smb = None;
                // Plan-03 path: "skipped …: reason", or give up when the
                // failures span the playlist.
                self.on_error(PlayError::Failed(error));
            }
        }
    }

    /// Load a downloaded file the track was waiting for, at the pending start
    /// offset. The engine's pause flag survives a `Load`, so the state the
    /// user left (paused or playing) is re-asserted right after it.
    fn load_ready_file(&mut self, path: std::path::PathBuf) {
        let start = self.pending_start.unwrap_or(Duration::ZERO);
        let keep_paused = self.state == PlayState::Paused;
        self.engine.send(EngineCmd::Load { path, start });
        self.engine.send(if keep_paused {
            EngineCmd::Pause
        } else {
            EngineCmd::Resume
        });
    }

    /// Halt playback but keep the current track selected (Stop semantics).
    fn stop_playback(&mut self) {
        // Plan 09: drop the queued next track first — the engine's `Stop`
        // takes the whole stream (and with it the queue) anyway, but the
        // explicit `Unqueue` keeps UI intent and engine state aligned.
        self.engine.send(EngineCmd::Unqueue);
        self.engine.send(EngineCmd::Stop);
        self.state = PlayState::Stopped;
        self.format = FormatInfo::blank();
        self.handle = None;
        self.pending_start = None;
        self.pending_advance = None;
        // Nothing is playing, so nothing waits for a download either: a
        // late `Ready` for it must be ignored, not loaded.
        self.pending_remote = None;
        self.pending_smb = None;
    }

    /// Bars and peak caps while nothing is analysed (plan §step 6): paused
    /// decays them (`×0.75`), stopped zeroes everything. Runs on every tick,
    /// since it costs nothing; the playing case is [`RealPlayer::analyze`].
    fn decay_spectrum(&mut self) {
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
            PlayState::Playing => {}
        }
    }

    /// Analyse the tap and smooth the result into the bars (plan §step 6):
    /// instant attack, release `0.45`, peak caps falling at `0.06` — the
    /// exact constants of `MockPlayer::tick`. Only while playing. The FFT is
    /// the expensive part, so the UI calls this once per frame it draws
    /// rather than on every tick (input, remote commands and poll wake-ups
    /// would otherwise each run a transform nobody sees).
    pub fn analyze(&mut self) {
        if self.state != PlayState::Playing {
            return;
        }
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

    /// Keep the play order in step with the playlist (and the shuffle flag)
    /// after any edit or shuffle toggle — and the cache window in step with
    /// both (it follows *play* order, plan 08).
    fn rebuild_order(&mut self) {
        let current = self.playlist.current();
        self.order
            .rebuild(self.playlist.len(), current, self.shuffle, &mut self.rng);
        self.update_cache_window();
    }
}

impl Default for RealPlayer {
    fn default() -> Self {
        Self::new(None, None)
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

    /// `(player, command log, event queue, shared DSP params)`.
    type Harness = (
        RealPlayer,
        Arc<Mutex<Vec<EngineCmd>>>,
        Arc<Mutex<Vec<EngineEvent>>>,
        Arc<SharedParams>,
    );

    fn fake_player() -> Harness {
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let engine = FakeEngine {
            cmds: Arc::clone(&cmds),
            events: Arc::clone(&events),
        };
        let params = Arc::new(SharedParams::new());
        (
            RealPlayer::with_engine(Box::new(engine), Arc::clone(&params)),
            cmds,
            events,
            params,
        )
    }

    fn tracks(n: usize) -> Vec<Track> {
        (0..n)
            .map(|i| Track {
                id: Track::next_id(),
                source: TrackSource::Local(PathBuf::from(format!("/m/{i}.flac"))),
                artist: Some("A".into()),
                title: Some(format!("Track {i}")),
                duration: None,
                bitrate_kbps: None,
                pos: Default::default(),
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
        let (mut player, cmds, events, params) = fake_player();
        player.handle(PlayerCommand::AddTracks(tracks(3)));
        assert!(loads_of(&cmds).is_empty(), "Add must not autoplay");
        (player, cmds, events, params)
    }

    /// `duration: None` so `on_started` leaves the playlist alone; seek
    /// tests that need a known length override it with `..fake_started()`.
    fn fake_started() -> StreamInfo {
        StreamInfo {
            codec: "FLAC".into(),
            lossless: true,
            f32_exact: true,
            sample_rate: 44_100,
            channels: 2,
            bit_depth: Some(16),
            duration: None,
        }
    }

    /// A handle reporting `played_frames` frames into the stream, with no
    /// gapless boundary recorded (`NO_BOUNDARY`).
    fn fake_handle(played_frames: u64) -> PlaybackHandle {
        PlaybackHandle {
            played: Arc::new(AtomicU64::new(played_frames)),
            boundary: Arc::new(AtomicU64::new(NO_BOUNDARY)),
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
        let (mut player, cmds, _events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, _events, ..) = three_tracks();
        player.handle(PlayerCommand::SeekBy(30));
        assert!(loads_of(&cmds).is_empty(), "nothing to seek when stopped");
    }

    #[test]
    fn an_unsupported_seek_restarts_the_track_with_a_notice() {
        let (mut player, cmds, events, ..) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(10 * 44_100));
        player.tick(Duration::ZERO);
        player.handle(PlayerCommand::SeekBy(5));
        queue(&events, EngineEvent::Error(PlayError::SeekUnsupported));
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
        let (mut player, cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, events, ..) = three_tracks();
        player.handle(PlayerCommand::Play);
        for _ in 0..3 {
            queue(&events, EngineEvent::Error("bad file".into()));
            player.tick(Duration::ZERO);
        }
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(load_paths(&cmds).len(), 3, "two skips, then give up");
        // The give-up notice keeps the last reason (device + why).
        assert_eq!(
            player.notice.as_deref(),
            Some("no playable tracks: bad file")
        );
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Stop)));
    }

    #[test]
    fn started_fills_the_format_line_and_blanks_it_on_stop() {
        let (mut player, _cmds, events, ..) = three_tracks();
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
        assert!(format.lossless);
        assert!(format.bit_perfect, "lossless at 100 % / centered");
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
        let (mut player, _cmds, events, ..) = three_tracks();
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
        let (mut player, _cmds, events, ..) = three_tracks();
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
        let (mut player, _cmds, events, ..) = three_tracks();
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
        let (mut player, cmds, _events, _params) = fake_player();
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
        let (mut player, cmds, _events, ..) = three_tracks();
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
        let (mut player, cmds, _events, ..) = three_tracks();
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
        let (mut player, cmds, _events, ..) = three_tracks();
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
        let (mut player, cmds, _events, ..) = three_tracks();
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

    // --- volume/balance → shared DSP targets (plan 05) ---

    /// Read one of the shared targets back as an f32.
    fn gain_at(params: &SharedParams, which: &str) -> f32 {
        let bits = match which {
            "volume" => params.volume_gain.load(Ordering::Relaxed),
            "left" => params.balance_left.load(Ordering::Relaxed),
            "right" => params.balance_right.load(Ordering::Relaxed),
            other => panic!("unknown target {other}"),
        };
        f32::from_bits(bits)
    }

    #[test]
    fn volume_and_balance_write_the_shared_dsp_targets() {
        let (mut player, _cmds, _events, params) = three_tracks();
        // Defaults: everything unity.
        assert_eq!(gain_at(&params, "volume"), 1.0);
        assert_eq!(
            (gain_at(&params, "left"), gain_at(&params, "right")),
            (1.0, 1.0)
        );

        // Volume 50 → gain 0.25 (the squared law), balance untouched.
        player.handle(PlayerCommand::SetVolume(50));
        assert_eq!(gain_at(&params, "volume"), 0.25);
        assert_eq!(
            (gain_at(&params, "left"), gain_at(&params, "right")),
            (1.0, 1.0)
        );

        // Balance +50 → only the left channel is attenuated.
        player.handle(PlayerCommand::SetBalance(50));
        assert_eq!(gain_at(&params, "volume"), 0.25, "volume stays");
        assert_eq!(
            (gain_at(&params, "left"), gain_at(&params, "right")),
            (0.5, 1.0)
        );

        // Back to 100 / 0 → exactly unity again (bit-perfect fast path).
        player.handle(PlayerCommand::SetVolume(100));
        player.handle(PlayerCommand::SetBalance(0));
        assert_eq!(gain_at(&params, "volume"), 1.0);
        assert_eq!(
            (gain_at(&params, "left"), gain_at(&params, "right")),
            (1.0, 1.0)
        );
    }

    #[test]
    fn balance_clamps_and_works_with_an_empty_playlist() {
        // The real session restores the config before any track is loaded,
        // so these must land on an empty player too. (`App` clamps the ±5
        // arithmetic; the command itself carries an in-range i8.)
        let (mut player, _cmds, _events, params) = fake_player();
        player.handle(PlayerCommand::SetBalance(127)); // out of range, fits i8
        assert_eq!(player.snapshot().balance, 100);
        assert_eq!(gain_at(&params, "left"), 0.0);
        player.handle(PlayerCommand::SetBalance(-100));
        assert_eq!(player.snapshot().balance, -100);
        assert_eq!(gain_at(&params, "right"), 0.0);
        assert_eq!(gain_at(&params, "left"), 1.0, "only the opposite side");
    }

    #[test]
    fn bit_perfect_disappears_below_full_volume_or_off_center() {
        let (mut player, _cmds, events, _params) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert!(player.snapshot().format.bit_perfect, "lossless at 100 / 0");

        player.handle(PlayerCommand::SetVolume(95));
        assert!(
            !player.snapshot().format.bit_perfect,
            "volume below 100 touches the samples"
        );
        player.handle(PlayerCommand::SetVolume(100));
        player.handle(PlayerCommand::SetBalance(5));
        assert!(
            !player.snapshot().format.bit_perfect,
            "an off-center balance touches the samples"
        );
        player.handle(PlayerCommand::SetBalance(0));
        assert!(
            player.snapshot().format.bit_perfect,
            "back to 100 / 0: bit-perfect again"
        );
    }

    #[test]
    fn a_source_f32_cannot_hold_is_lossless_but_not_bit_perfect() {
        let (mut player, _cmds, events, _params) = three_tracks();
        player.handle(PlayerCommand::Play);
        let mut deep = fake_started();
        deep.codec = "PCM".into();
        deep.bit_depth = Some(32);
        deep.f32_exact = false;
        queue(&events, EngineEvent::Started(deep, fake_handle(0)));
        player.tick(Duration::ZERO);
        let format = player.snapshot().format;
        assert!(format.lossless, "still a lossless codec");
        assert!(!format.bit_perfect, "rounded to f32 on the way in");
    }

    #[test]
    fn a_lossy_source_is_never_bit_perfect() {
        let (mut player, _cmds, events, _params) = three_tracks();
        player.handle(PlayerCommand::Play);
        let mut lossy = fake_started();
        lossy.codec = "MP3".into();
        lossy.lossless = false;
        lossy.bit_depth = None;
        queue(&events, EngineEvent::Started(lossy, fake_handle(0)));
        player.tick(Duration::ZERO);
        let format = player.snapshot().format;
        assert!(!format.bit_perfect, "lossy, even at 100 / 0");
        assert!(!format.lossless);
    }

    #[test]
    fn swap_channels_is_forwarded_and_clears_bit_perfect() {
        let (mut player, _cmds, events, params) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert!(player.snapshot().format.bit_perfect, "default: no swap");

        player.handle(PlayerCommand::SetSwapChannels(true));
        assert!(params.swap_channels.load(Ordering::Relaxed), "flag set");
        assert!(
            !player.snapshot().format.bit_perfect,
            "a reorder is processing: marker off"
        );

        player.handle(PlayerCommand::SetSwapChannels(false));
        assert!(!params.swap_channels.load(Ordering::Relaxed));
        assert!(
            player.snapshot().format.bit_perfect,
            "swap off: marker back"
        );
    }

    // --- EQ → shared DSP targets (plan 06) ---

    #[test]
    fn set_eq_writes_the_shared_targets_and_bumps_the_version() {
        let (mut player, _cmds, _events, params) = three_tracks();
        let before = params.eq_version.load(Ordering::Relaxed);
        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [3, 6, 0, 0, 0, 0, 0, 0, 0, 0, -6],
        });
        let gains: Vec<i8> = params
            .eq_gains
            .iter()
            .map(|g| g.load(Ordering::Relaxed) as i8)
            .collect();
        assert_eq!(gains, vec![3, 6, 0, 0, 0, 0, 0, 0, 0, 0, -6]);
        assert!(params.eq_enabled.load(Ordering::Relaxed), "flag set");
        assert!(
            params.eq_version.load(Ordering::Relaxed) > before,
            "version bumped after the writes"
        );
    }

    #[test]
    fn set_eq_works_with_an_empty_playlist_and_clamps_out_of_range() {
        // The session restores `[eq]` at startup, before any track loads.
        let (mut player, _cmds, _events, params) = fake_player();
        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [20, -30, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        });
        let gains: Vec<i8> = params
            .eq_gains
            .iter()
            .map(|g| g.load(Ordering::Relaxed) as i8)
            .collect();
        assert_eq!(gains, vec![12, -12, 0, 0, 0, 0, 0, 0, 0, 0, 0], "clamped");
    }

    #[test]
    fn an_active_eq_clears_the_bit_perfect_marker() {
        let (mut player, _cmds, events, _params) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert!(player.snapshot().format.bit_perfect, "default: flat EQ");

        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [0, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        });
        assert!(
            !player.snapshot().format.bit_perfect,
            "EQ on and not flat: processing"
        );
        // On but flat: passes through untouched, marker back.
        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [0; 11],
        });
        assert!(
            player.snapshot().format.bit_perfect,
            "flat EQ passes through"
        );
        // Off with non-zero gains: also bypassed.
        player.handle(PlayerCommand::SetEq {
            enabled: false,
            gains: [12; 11],
        });
        assert!(
            player.snapshot().format.bit_perfect,
            "EQ off passes through"
        );
    }

    #[test]
    fn started_copies_the_tagged_bitrate_for_lossy_tracks() {
        let (mut player, _cmds, events, _params) = fake_player();
        let mut list = tracks(1);
        list[0].bitrate_kbps = Some(320);
        player.handle(PlayerCommand::ReplaceTracks(list));
        let mut lossy = fake_started();
        lossy.codec = "MP3".into();
        lossy.lossless = false;
        lossy.bit_depth = None;
        queue(&events, EngineEvent::Started(lossy, fake_handle(0)));
        player.tick(Duration::ZERO);
        let format = player.snapshot().format;
        assert_eq!(format.bitrate_kbps, Some(320));
        assert_eq!(format.describe(), "MP3  320kbps  44.1kHz  stereo");
        // A lossless source never carries a bitrate into the line.
        player.handle(PlayerCommand::PlayTrack(0));
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert_eq!(player.snapshot().format.bitrate_kbps, None);
    }

    #[test]
    fn shuffled_next_covers_the_whole_playlist_before_repeating() {
        let (mut player, _cmds, events, _params) = fake_player();
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

    // --- gapless: queue maintenance + the boundary clock (plan 09) ---

    /// Every `Queue`, as `(source, eq_bypass)` pairs.
    fn queues_of(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<(QueueSource, bool)> {
        log_of(cmds)
            .into_iter()
            .filter_map(|cmd| match cmd {
                EngineCmd::Queue {
                    source,
                    start,
                    eq_bypass,
                } => {
                    assert_eq!(start, Duration::ZERO, "a gapless queue starts at 0");
                    Some((source, eq_bypass))
                }
                _ => None,
            })
            .collect()
    }

    /// Every queued *local* path (remote sources are asserted separately).
    fn queued_paths(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<PathBuf> {
        queues_of(cmds)
            .into_iter()
            .filter_map(|(source, _)| match source {
                QueueSource::Local(path) => Some(path),
                QueueSource::Remote { .. } | QueueSource::Smb(_) => None,
            })
            .collect()
    }

    #[test]
    fn the_next_track_is_queued_after_started_and_requeued_on_order_changes() {
        let (mut player, cmds, events, ..) = three_tracks();
        player.handle(PlayerCommand::Play);
        assert!(
            queues_of(&cmds).is_empty(),
            "nothing queued before the engine reports Started"
        );

        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert_eq!(
            queued_paths(&cmds),
            [PathBuf::from("/m/1.flac")],
            "upcoming(1) is pre-opened right after Started"
        );

        // A shuffle toggle re-queues whatever the new order puts next.
        player.handle(PlayerCommand::ToggleShuffle);
        let queued = queued_paths(&cmds);
        assert_eq!(queued.len(), 2, "re-sent after the order changed");
        assert_ne!(
            queued[1],
            PathBuf::from("/m/0.flac"),
            "never the track already playing"
        );

        // The repeat toggle and a playlist edit re-send it too (plan §step 5).
        player.handle(PlayerCommand::ToggleRepeat);
        assert_eq!(queued_paths(&cmds).len(), 3, "re-sent on the repeat toggle");
        player.handle(PlayerCommand::AddTracks(tracks(1)));
        assert_eq!(
            queued_paths(&cmds).len(),
            4,
            "re-sent after a playlist edit"
        );
    }

    #[test]
    fn end_of_order_and_a_stopped_player_queue_nothing() {
        let (mut player, cmds, events, ..) = three_tracks();
        player.handle(PlayerCommand::PlayTrack(2)); // the last track
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert!(
            queued_paths(&cmds).is_empty(),
            "no next track after the end of the order"
        );
        assert!(matches!(log_of(&cmds).last(), Some(EngineCmd::Unqueue)));

        // Stop drops the queue *before* stopping the stream (plan §Seek/Stop).
        player.handle(PlayerCommand::PlayTrack(0));
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert_eq!(queued_paths(&cmds), [PathBuf::from("/m/1.flac")]);
        player.handle(PlayerCommand::Stop);
        let log = log_of(&cmds);
        let unqueue = log
            .iter()
            .rposition(|c| matches!(c, EngineCmd::Unqueue))
            .expect("Stop goes with an Unqueue");
        let stop = log
            .iter()
            .rposition(|c| matches!(c, EngineCmd::Stop))
            .expect("Stop");
        assert!(unqueue < stop, "the queue is dropped before Stop: {log:?}");

        // …and a stopped player never re-queues, even when the order changes.
        let queued_before = queued_paths(&cmds).len();
        player.handle(PlayerCommand::AddTracks(tracks(1)));
        assert_eq!(
            queued_paths(&cmds).len(),
            queued_before,
            "stopped: no queue traffic"
        );
    }

    /// Plan 09, step 4: the `Advanced` event may precede the audio by the
    /// ring's length — `current`, the clock and the format line move only
    /// when `played` really crosses `boundary`, exactly at that frame.
    #[test]
    fn advanced_moves_the_current_track_exactly_at_the_boundary() {
        let (mut player, cmds, events, ..) = three_tracks();
        player.handle(PlayerCommand::Play);
        let played = Arc::new(AtomicU64::new(0));
        let boundary = Arc::new(AtomicU64::new(110_250)); // 2.5 s in
        let handle = PlaybackHandle {
            played: Arc::clone(&played),
            boundary: Arc::clone(&boundary),
            sample_rate: 44_100,
            start_offset: Duration::ZERO,
            tap: RingBuffer::<f32>::new(64).1,
        };
        queue(&events, EngineEvent::Started(fake_started(), handle));
        player.tick(Duration::ZERO);
        assert_eq!(player.playlist.current(), Some(0));

        // The engine switched decoders; the audio has not arrived yet.
        let mut next_info = fake_started();
        next_info.codec = "MP3".into();
        next_info.lossless = false;
        next_info.bit_depth = None;
        queue(&events, EngineEvent::Advanced(next_info));
        player.tick(Duration::ZERO);
        assert_eq!(
            player.playlist.current(),
            Some(0),
            "the event alone moves nothing"
        );
        assert_eq!(player.snapshot().format.codec, "FLAC", "not yet");

        // One frame short of the boundary: still the old track.
        played.store(110_249, Ordering::Relaxed);
        player.tick(Duration::ZERO);
        assert_eq!(player.playlist.current(), Some(0), "one frame early");

        // Exactly the boundary frame: the track change happens now.
        played.store(110_250, Ordering::Relaxed);
        player.tick(Duration::ZERO);
        assert_eq!(player.playlist.current(), Some(1), "crossed");
        let snapshot = player.snapshot();
        assert_eq!(snapshot.format.codec, "MP3", "format line switched");
        assert_eq!(
            snapshot.position,
            Duration::ZERO,
            "the new track counts from 0, not from the stream's start"
        );

        // The clock keeps counting from the boundary (not from frame 0).
        played.store(110_250 + 44_100, Ordering::Relaxed);
        assert_eq!(player.snapshot().position, Duration::from_secs(1));

        // The queue moved on to the track after that.
        assert_eq!(
            queued_paths(&cmds).last(),
            Some(&PathBuf::from("/m/2.flac")),
            "the boundary crossing re-queues for the next one"
        );
    }

    /// The `stream_pure` bypass rides inside the `Queue` (plan 09, step 6):
    /// the engine flips the shared flag at the boundary, so the flag must
    /// belong to the *queued* track, not to the current one.
    #[test]
    fn stream_pure_carries_the_bypass_flag_into_the_queue() {
        let (mut player, cmds, events, _params) = fake_player();
        player.jf_mode = Some(JellyfinMode::StreamPure);
        player.handle(PlayerCommand::AddTracks(vec![
            remote_track("abc"),
            remote_track("def"),
        ]));
        player.handle(PlayerCommand::AddTracks(tracks(2)));

        // A remote next track → queued with the bypass set.
        player.handle(PlayerCommand::PlayTrack(0));
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        let queued = queues_of(&cmds);
        let last = queued.last().expect("the next track is queued");
        assert_eq!(
            last.0,
            QueueSource::Remote {
                item_id: "def".into(),
                container: Some("flac".into()),
            },
            "stream mode pre-opens the HTTP source itself"
        );
        assert!(last.1, "queued remote track in stream_pure: bypass set");

        // A local next track → queued without it.
        player.handle(PlayerCommand::PlayTrack(2));
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        let queued = queues_of(&cmds);
        let last = queued.last().expect("the next track is queued");
        assert_eq!(last.0, QueueSource::Local(PathBuf::from("/m/1.flac")));
        assert!(!last.1, "a local next track keeps the EQ working");
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
        let (mut player, _cmds, events, ..) = three_tracks();
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

    #[test]
    fn playing_bars_move_only_when_analysed() {
        let (mut player, _cmds, events, ..) = three_tracks();
        player.handle(PlayerCommand::Play);
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        player.spectrum.iter_mut().for_each(|b| *b = 1.0);
        player.tick(Duration::ZERO);
        assert!(
            player.snapshot().spectrum.iter().all(|v| *v == 1.0),
            "a tick runs no analysis"
        );
        // An empty tap releases toward zero by `RELEASE` (0.45).
        player.analyze();
        let released = player.snapshot().spectrum[0];
        assert!((released - 0.45).abs() < 1e-6, "one analysis: {released}");
    }

    // --- cache mode: buffering, ready and failure (plan 08, step 5) ---

    /// A remote (Jellyfin) track for the mixed-list tests.
    fn remote_track(id: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Remote {
                item_id: id.into(),
                container: Some("flac".into()),
            },
            artist: Some("A".into()),
            title: Some(format!("R {id}")),
            duration: None,
            bitrate_kbps: None,
            pos: Default::default(),
        }
    }

    /// A client for a server that answers nothing (no window is ever
    /// *really* downloaded in these state-machine tests — events are
    /// injected through `handle_cache_event`).
    /// The track ids along the play order: what already played (up to and
    /// including the current track), and what is queued after it.
    fn shuffle_view(player: &RealPlayer) -> (Vec<u64>, Vec<u64>) {
        let tracks = player.playlist.tracks();
        let len = tracks.len();
        let mut played: Vec<u64> = player
            .order
            .previous(len)
            .collect::<Vec<_>>()
            .iter()
            .rev()
            .map(|&i| tracks[i].id)
            .collect();
        if let Some(current) = player.playlist.current() {
            played.push(tracks[current].id);
        }
        let upcoming = player.order.upcoming(len).map(|i| tracks[i].id).collect();
        (played, upcoming)
    }

    /// Audit P3: in shuffle, editing the playlist must not reshuffle what is
    /// already queued — history, the current track and the upcoming order
    /// survive appends (each Jellyfin page is one), removals and moves, so
    /// the Jellyfin cache window does not churn.
    #[test]
    fn shuffle_keeps_its_order_across_playlist_edits() {
        let (mut player, ..) = fake_player();
        player.handle(PlayerCommand::AddTracks(tracks(12)));
        player.handle(PlayerCommand::ToggleShuffle);
        player.handle(PlayerCommand::Play);
        player.handle(PlayerCommand::Next);
        player.handle(PlayerCommand::Next);
        let (played, upcoming) = shuffle_view(&player);
        assert_eq!(played.len(), 3);

        // Append: old upcoming keeps its relative order, new ids join it.
        let added = tracks(5);
        let added_ids: Vec<u64> = added.iter().map(|t| t.id).collect();
        player.handle(PlayerCommand::AppendInOrder(added));
        let (played_after, upcoming_after) = shuffle_view(&player);
        assert_eq!(
            played_after, played,
            "history and current survive an append"
        );
        let old_only: Vec<u64> = upcoming_after
            .iter()
            .copied()
            .filter(|id| !added_ids.contains(id))
            .collect();
        assert_eq!(old_only, upcoming, "queued order survives an append");
        assert_eq!(
            upcoming_after.len(),
            upcoming.len() + 5,
            "new tracks queued"
        );

        // Remove a queued track: everything else keeps its place.
        let victim = upcoming_after[1];
        let index = player
            .playlist
            .tracks()
            .iter()
            .position(|t| t.id == victim)
            .unwrap();
        player.handle(PlayerCommand::RemoveTrack(index));
        let (played_now, upcoming_now) = shuffle_view(&player);
        assert_eq!(played_now, played);
        let expected: Vec<u64> = upcoming_after
            .iter()
            .copied()
            .filter(|&id| id != victim)
            .collect();
        assert_eq!(upcoming_now, expected, "a removal only drops that track");

        // Move a track in the playlist: play order is by track, not by row.
        player.handle(PlayerCommand::MoveTrack { from: 4, up: true });
        assert_eq!(
            shuffle_view(&player),
            (played_now, upcoming_now),
            "a move changes nothing"
        );
    }

    /// Audit S6: crash leftovers are swept at startup in *every* mode —
    /// a stream-mode session can leave a fallback dir behind too.
    #[test]
    fn startup_sweeps_stale_dirs_in_stream_mode_too() {
        let parent = tempfile::tempdir().unwrap();
        let stale = parent.path().join("ampwire-jellyfin-99999");
        std::fs::create_dir(&stale).unwrap();
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(25 * 3600))
            .unwrap();
        let config = JellyfinConfig {
            url: "http://127.0.0.1:9".into(),
            api_key: "k".into(),
            api_key_file: None,
            user: None,
            mode: JellyfinMode::Stream,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: Some(parent.path().to_path_buf()),
            notice: None,
        };
        let player = RealPlayer::new(Some(&config), None); // no device until a Load
        assert!(!stale.exists(), "stale leftover swept in stream mode");
        assert!(player.cache.is_none(), "still no cache dir in stream mode");
    }

    fn idle_client() -> Client {
        Client::new(&JellyfinConfig {
            url: "http://127.0.0.1:9".into(),
            api_key: "k".into(),
            api_key_file: None,
            user: None,
            mode: JellyfinMode::Cache,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
            notice: None,
        })
    }

    /// `(player, …)` with a real (idle) cache directory under `dir`.
    fn cached_player(dir: &tempfile::TempDir) -> Harness {
        let (mut player, cmds, events, params) = fake_player();
        player.cache =
            Some(Cache::new(idle_client(), Some(dir.path().to_path_buf())).expect("cache dir"));
        player.jf_mode = Some(JellyfinMode::Cache);
        (player, cmds, events, params)
    }

    #[test]
    fn a_ready_cache_file_plays_and_seeks_like_a_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = cached_player(&dir);
        let path = dir.path().join("abc.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player
            .cache
            .as_mut()
            .unwrap()
            .insert_ready("abc", path.clone());

        player.handle(PlayerCommand::AddTracks(vec![remote_track("abc")]));
        player.handle(PlayerCommand::Play);
        assert_eq!(
            load_paths(&cmds),
            std::slice::from_ref(&path),
            "a cache hit is a plain local Load"
        );
        assert_eq!(player.state, PlayState::Playing);
        assert_eq!(
            player.take_notice(),
            None,
            "ready at once: no buffering notice"
        );

        // Seeking is a normal local-file seek from here on.
        player.handle(PlayerCommand::SeekBy(5));
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(path, Duration::from_secs(5)))
        );
    }

    #[test]
    fn a_remote_track_buffers_until_the_cache_reports_ready() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = cached_player(&dir);
        player.handle(PlayerCommand::AddTracks(vec![remote_track("abc")]));
        player.handle(PlayerCommand::AddTracks(tracks(1)));

        player.handle(PlayerCommand::PlayTrack(0));
        assert_eq!(player.state, PlayState::Playing, "playing while buffering");
        assert_eq!(
            player.take_notice().as_deref(),
            Some("buffering…"),
            "the user sees why there is no sound yet"
        );
        assert_eq!(
            player.pending_remote.as_ref().map(ItemId::as_str),
            Some("abc")
        );
        assert!(load_paths(&cmds).is_empty(), "nothing sent to the engine");

        // Paused while buffering: the Ready load re-asserts the pause.
        player.handle(PlayerCommand::Pause);
        assert_eq!(player.state, PlayState::Paused);

        // The downloader finishes the file.
        let path = dir.path().join("abc.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player.handle_cache_event(CacheEvent::Ready {
            item_id: "abc".into(),
            path: path.clone(),
        });
        assert_eq!(load_paths(&cmds), [path], "Load once the file exists");
        assert!(
            matches!(log_of(&cmds).last(), Some(EngineCmd::Pause)),
            "the pause survives the deferred load: {cmds:?}",
            cmds = log_of(&cmds)
        );
        assert_eq!(player.pending_remote, None, "no longer waiting");
        assert_eq!(player.state, PlayState::Paused);
    }

    #[test]
    fn a_ready_for_another_item_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = cached_player(&dir);
        player.handle(PlayerCommand::AddTracks(vec![remote_track("abc")]));
        player.handle(PlayerCommand::Play);
        assert_eq!(
            player.pending_remote.as_ref().map(ItemId::as_str),
            Some("abc")
        );

        let other = dir.path().join("zzz.flac");
        std::fs::write(&other, b"pretend flac").unwrap();
        player.handle_cache_event(CacheEvent::Ready {
            item_id: "zzz".into(),
            path: other,
        });
        assert!(
            load_paths(&cmds).is_empty(),
            "a prefetch finishing must not interrupt the buffering track"
        );
        assert_eq!(
            player.pending_remote.as_ref().map(ItemId::as_str),
            Some("abc")
        );
    }

    #[test]
    fn a_failed_download_takes_the_skip_path() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = cached_player(&dir);
        player.handle(PlayerCommand::AddTracks(vec![remote_track("abc")]));
        player.handle(PlayerCommand::AddTracks(tracks(2)));
        player.handle(PlayerCommand::Play);
        assert_eq!(
            player.pending_remote.as_ref().map(ItemId::as_str),
            Some("abc")
        );

        player.handle_cache_event(CacheEvent::Failed {
            item_id: "abc".into(),
            error: "jellyfin: cannot reach server".into(),
        });
        assert_eq!(
            player.take_notice().as_deref(),
            Some("skipped A - R abc: jellyfin: cannot reach server"),
            "plan-03 error path with the reason kept"
        );
        assert_eq!(load_paths(&cmds).last(), Some(&PathBuf::from("/m/0.flac")));
        assert_eq!(player.state, PlayState::Playing, "moved on to the next");
        assert_eq!(player.pending_remote, None);

        // A late `Ready` for the abandoned item changes nothing.
        player.handle_cache_event(CacheEvent::Ready {
            item_id: "abc".into(),
            path: dir.path().join("abc.flac"),
        });
        assert_eq!(
            load_paths(&cmds).len(),
            1,
            "only the skip's load: the stale Ready adds nothing"
        );
    }

    #[test]
    fn stop_while_buffering_makes_a_late_ready_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = cached_player(&dir);
        player.handle(PlayerCommand::AddTracks(vec![remote_track("abc")]));
        player.handle(PlayerCommand::Play);
        assert_eq!(
            player.pending_remote.as_ref().map(ItemId::as_str),
            Some("abc")
        );

        player.handle(PlayerCommand::Stop);
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(player.pending_remote, None, "nothing waits after Stop");

        let path = dir.path().join("abc.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player.handle_cache_event(CacheEvent::Ready {
            item_id: "abc".into(),
            path,
        });
        assert!(
            load_paths(&cmds).is_empty(),
            "a stale Ready must never start playback"
        );
    }

    // --- SMB cache mode: buffering, ready and failure (plan 10, step 5) ---

    /// An SMB row. The server is always loopback here, so the background
    /// downloader's own connect fails instantly instead of holding
    /// `SmbCache::drop` on a DNS lookup.
    fn smb_url(path: &str) -> SmbUrl {
        SmbUrl {
            server: "127.0.0.1".into(),
            share: "Music".into(),
            path: path.into(),
        }
    }

    fn smb_row(url: SmbUrl) -> Track {
        let title = url.path.clone();
        Track {
            id: Track::next_id(),
            source: TrackSource::Smb { url },
            artist: Some("A".into()),
            title: Some(format!("S {title}")),
            duration: None,
            bitrate_kbps: None,
            pos: Default::default(),
        }
    }

    /// `(player, …)` with a real (idle) SMB window directory under `dir`.
    fn smb_cached_player(dir: &tempfile::TempDir) -> Harness {
        let (mut player, cmds, events, params) = fake_player();
        player.smb_cache = Some(
            SmbCache::new(
                crate::smb::SmbClientCfg::new("127.0.0.1", "", ""),
                Some(dir.path().to_path_buf()),
            )
            .expect("smb cache dir"),
        );
        (player, cmds, events, params)
    }

    #[test]
    fn a_ready_smb_file_plays_and_seeks_like_a_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = smb_cached_player(&dir);
        let url = smb_url("album/song.flac");
        let path = dir.path().join("cached.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player
            .smb_cache
            .as_mut()
            .expect("window")
            .insert_ready(&url, path.clone());

        player.handle(PlayerCommand::AddTracks(vec![smb_row(url)]));
        player.handle(PlayerCommand::Play);
        assert_eq!(
            load_paths(&cmds),
            std::slice::from_ref(&path),
            "a cache hit is a plain local Load"
        );
        assert_eq!(player.state, PlayState::Playing);
        assert_eq!(
            player.take_notice(),
            None,
            "ready at once: no buffering notice"
        );

        // Seeking is a normal local-file seek from here on.
        player.handle(PlayerCommand::SeekBy(5));
        assert_eq!(
            loads_of(&cmds).last(),
            Some(&(path, Duration::from_secs(5)))
        );
    }

    #[test]
    fn an_smb_track_buffers_until_the_window_reports_ready() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = smb_cached_player(&dir);
        let url = smb_url("album/song.flac");
        let row = smb_row(url.clone());
        player.handle(PlayerCommand::AddTracks(vec![row]));
        player.handle(PlayerCommand::AddTracks(tracks(1)));

        player.handle(PlayerCommand::PlayTrack(0));
        assert_eq!(player.state, PlayState::Playing, "playing while buffering");
        assert_eq!(
            player.take_notice().as_deref(),
            Some("buffering…"),
            "the user sees why there is no sound yet"
        );
        assert_eq!(player.pending_smb.as_ref(), Some(&url));
        assert!(load_paths(&cmds).is_empty(), "nothing sent to the engine");
        assert!(
            player.smb_cache.is_some(),
            "the first SMB track created the window"
        );

        // Paused while buffering: the Ready load re-asserts the pause.
        player.handle(PlayerCommand::Pause);
        assert_eq!(player.state, PlayState::Paused);

        // The downloader finishes the file.
        let path = dir.path().join("cached.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player.handle_smb_cache_event(SmbCacheEvent::Ready {
            url: url.clone(),
            path: path.clone(),
        });
        assert_eq!(load_paths(&cmds), [path], "Load once the file exists");
        assert!(
            matches!(log_of(&cmds).last(), Some(EngineCmd::Pause)),
            "the pause survives the deferred load: {:?}",
            log_of(&cmds)
        );
        assert_eq!(player.pending_smb, None, "no longer waiting");
        assert_eq!(player.state, PlayState::Paused);
    }

    #[test]
    fn an_smb_ready_for_another_url_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = smb_cached_player(&dir);
        let url = smb_url("album/song.flac");
        player.handle(PlayerCommand::AddTracks(vec![smb_row(url.clone())]));
        player.handle(PlayerCommand::Play);
        assert_eq!(player.pending_smb.as_ref(), Some(&url));

        let other = smb_url("album/other.flac");
        let path = dir.path().join("zzz.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player.handle_smb_cache_event(SmbCacheEvent::Ready { url: other, path });
        assert!(
            load_paths(&cmds).is_empty(),
            "a prefetch finishing must not interrupt the buffering track"
        );
        assert_eq!(player.pending_smb.as_ref(), Some(&url));
    }

    #[test]
    fn an_smb_failed_download_takes_the_skip_path() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = smb_cached_player(&dir);
        let url = smb_url("album/song.flac");
        let row = smb_row(url.clone());
        let expected = format!("skipped {}: smb: cannot connect", row.display_name());
        player.handle(PlayerCommand::AddTracks(vec![row]));
        player.handle(PlayerCommand::AddTracks(tracks(2)));
        player.handle(PlayerCommand::Play);
        assert_eq!(player.pending_smb.as_ref(), Some(&url));

        player.handle_smb_cache_event(SmbCacheEvent::Failed {
            url: url.clone(),
            error: "smb: cannot connect".into(),
        });
        assert_eq!(
            player.take_notice().as_deref(),
            Some(expected.as_str()),
            "plan-03 error path with the reason kept"
        );
        assert_eq!(load_paths(&cmds).last(), Some(&PathBuf::from("/m/0.flac")));
        assert_eq!(player.state, PlayState::Playing, "moved on to the next");
        assert_eq!(player.pending_smb, None);

        // A late `Ready` for the abandoned item changes nothing.
        player.handle_smb_cache_event(SmbCacheEvent::Ready {
            url,
            path: dir.path().join("cached.flac"),
        });
        assert_eq!(
            load_paths(&cmds).len(),
            1,
            "only the skip's load: the stale Ready adds nothing"
        );
    }

    #[test]
    fn stop_while_smb_buffering_makes_a_late_ready_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = smb_cached_player(&dir);
        let url = smb_url("album/song.flac");
        player.handle(PlayerCommand::AddTracks(vec![smb_row(url.clone())]));
        player.handle(PlayerCommand::Play);
        assert_eq!(player.pending_smb.as_ref(), Some(&url));

        player.handle(PlayerCommand::Stop);
        assert_eq!(player.state, PlayState::Stopped);
        assert_eq!(player.pending_smb, None, "nothing waits after Stop");

        let path = dir.path().join("cached.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player.handle_smb_cache_event(SmbCacheEvent::Ready { url, path });
        assert!(
            load_paths(&cmds).is_empty(),
            "a stale Ready must never start playback"
        );
    }

    /// Every `LoadSmb`, as `(url, start)` pairs — the SMB counterpart of
    /// `remotes_of`.
    fn smb_loads_of(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<(SmbUrl, Duration)> {
        log_of(cmds)
            .into_iter()
            .filter_map(|cmd| match cmd {
                EngineCmd::LoadSmb { url, start } => Some((url, start)),
                _ => None,
            })
            .collect()
    }

    /// Plan 10, step 6: stream mode hands the engine the URL (and a seek as
    /// a reload at the offset) — no cache directory, no player-side
    /// `buffering…` (the engine announces it), and the credentials never
    /// reach a `Track`.
    #[test]
    fn stream_mode_sends_load_smb_and_never_creates_a_cache() {
        let (mut player, cmds, ..) = fake_player();
        let config: crate::config::Config =
            toml::from_str("[smb]\nmode = \"stream\"\n").expect("config");
        player.smb = config.smb;

        let url = smb_url("album/song.flac");
        player.handle(PlayerCommand::AddTracks(vec![smb_row(url.clone())]));
        player.handle(PlayerCommand::Play);
        assert!(player.smb_cache.is_none(), "no window in stream mode");
        assert_eq!(player.pending_smb, None, "nothing waits on a cache");
        assert_eq!(
            player.take_notice(),
            None,
            "the engine is the one that announces buffering"
        );
        assert_eq!(
            smb_loads_of(&cmds),
            [(url.clone(), Duration::ZERO)],
            "one LoadSmb from the play"
        );

        // A seek re-opens the source at the offset — the SMB counterpart of
        // `LoadRemote` on a Jellyfin seek.
        player.handle(PlayerCommand::SeekBy(5));
        let loads = smb_loads_of(&cmds);
        assert_eq!(
            loads.last(),
            Some(&(url, Duration::from_secs(5))),
            "seek = reload at the new offset: {loads:?}"
        );
    }

    /// Plan 10, step 7: the gapless queue's view of an SMB *next* track —
    /// nothing while the window is still downloading it (the normal
    /// buffering path owns that moment), the local file once it is ready,
    /// and the URL itself in stream mode, where the engine pre-opens it on
    /// its own thread ahead of the boundary.
    ///
    /// (End-of-order and repeat-*wrap* queue nothing because `upcoming`
    /// never wraps — that is the play order's rule, covered by the local
    /// tests, and it is source-agnostic.)
    #[test]
    fn the_gapless_queue_waits_for_the_window_and_streams_a_url() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, cmds, ..) = smb_cached_player(&dir);
        let url = smb_url("album/next.flac");
        player.handle(PlayerCommand::AddTracks(tracks(1))); // 0: local, plays
        player.handle(PlayerCommand::AddTracks(vec![smb_row(url.clone())])); // 1: next
        player.handle(PlayerCommand::PlayTrack(0));
        assert!(
            queues_of(&cmds).is_empty(),
            "nothing queued while the SMB file is still downloading"
        );

        // The window finishes it: the next queue becomes a plain local path,
        // and the engine pre-opens it like any other file.
        let path = dir.path().join("cached.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player
            .smb_cache
            .as_mut()
            .expect("window")
            .insert_ready(&url, path.clone());
        player.refresh_queue();
        assert_eq!(
            queues_of(&cmds).last().map(|(source, _)| source),
            Some(&QueueSource::Local(path)),
            "a downloaded file queues as a local path"
        );

        // Stream mode: the queue carries the URL, credentials included in
        // the engine's own setup — never in the command.
        let config: crate::config::Config =
            toml::from_str("[smb]\nmode = \"stream\"\n").expect("config");
        player.smb = config.smb;
        player.refresh_queue();
        assert_eq!(
            queues_of(&cmds).last().map(|(source, _)| source),
            Some(&QueueSource::Smb(url)),
            "stream mode queues the URL itself"
        );
    }

    /// Every mutation that refreshes a cache window refreshes *both* of
    /// them (plan 10, step 5): the SMB window is re-projected from the
    /// playlist each time, so URLs that left it drop out of the bookkeeping.
    #[test]
    fn every_playlist_mutation_refreshes_the_smb_window_too() {
        let dir = tempfile::tempdir().unwrap();
        let (mut player, ..) = fake_player();
        player.smb_cache = Some(
            SmbCache::new(
                crate::smb::SmbClientCfg::new("127.0.0.1", "", ""),
                Some(dir.path().to_path_buf()),
            )
            .expect("window"),
        );
        let url = smb_url("album/song.flac");
        let path = dir.path().join("cached.flac");
        std::fs::write(&path, b"pretend flac").unwrap();
        player
            .smb_cache
            .as_mut()
            .expect("window")
            .insert_ready(&url, path.clone());
        assert_eq!(
            player
                .smb_cache
                .as_ref()
                .expect("window")
                .path_if_ready(&url),
            Some(path),
            "ready before any mutation"
        );

        // Any mutation re-projects the window from the playlist. This one
        // has no SMB row at all, so the URL leaves the window and the
        // bookkeeping goes with it — the same pruning the Jellyfin window
        // does, driven by the same call site.
        player.handle(PlayerCommand::AddTracks(tracks(1)));
        assert_eq!(
            player
                .smb_cache
                .as_ref()
                .expect("window")
                .path_if_ready(&url),
            None,
            "the window dropped a URL that is no longer wanted"
        );
    }

    // --- stream modes (plan 08, step 6) ---

    /// Every `LoadRemote`, as `(item_id, container, start)` triples.
    fn remotes_of(cmds: &Arc<Mutex<Vec<EngineCmd>>>) -> Vec<(String, Option<String>, Duration)> {
        log_of(cmds)
            .into_iter()
            .filter_map(|cmd| match cmd {
                EngineCmd::LoadRemote {
                    item_id,
                    container,
                    start,
                } => Some((item_id.to_string(), container, start)),
                _ => None,
            })
            .collect()
    }

    /// A player in `mode` with one remote + one local track, no cache.
    fn stream_player(mode: JellyfinMode) -> Harness {
        let (mut player, cmds, events, params) = fake_player();
        player.jf_mode = Some(mode);
        player.handle(PlayerCommand::AddTracks(vec![remote_track("abc")]));
        player.handle(PlayerCommand::AddTracks(tracks(1)));
        (player, cmds, events, params)
    }

    #[test]
    fn stream_mode_asks_the_engine_for_a_remote_load() {
        let (mut player, cmds, _events, ..) = stream_player(JellyfinMode::Stream);
        assert!(player.cache.is_none(), "no cache dir in stream mode");

        player.handle(PlayerCommand::PlayTrack(0));
        assert_eq!(
            remotes_of(&cmds),
            vec![("abc".to_string(), Some("flac".to_string()), Duration::ZERO)],
            "stream mode never touches the cache"
        );
        assert_eq!(player.state, PlayState::Playing);

        // A seek is the same remote load at a new offset (the engine's
        // `HttpRangeSource` re-opens at the cursor, no request on seek).
        player.handle(PlayerCommand::SeekBy(30));
        assert_eq!(
            remotes_of(&cmds).last(),
            Some(&(
                "abc".to_string(),
                Some("flac".to_string()),
                Duration::from_secs(30)
            ))
        );

        // Moving to the local track uses the plain path load again.
        player.handle(PlayerCommand::PlayTrack(1));
        assert_eq!(load_paths(&cmds).last(), Some(&PathBuf::from("/m/0.flac")));
    }

    #[test]
    fn buffering_event_notices_and_started_retires_it() {
        let (mut player, _cmds, events, ..) = stream_player(JellyfinMode::Stream);
        player.handle(PlayerCommand::PlayTrack(0));

        // The engine announces the blocking probe → the notice goes up
        // (the App drains it on its next frame; read it straight off the
        // player here so the retirement below can still be observed).
        queue(&events, EngineEvent::Buffering);
        player.tick(Duration::ZERO);
        assert_eq!(player.notice.as_deref(), Some("buffering…"));

        // The stream opens: the buffering notice must not linger.
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert_eq!(
            player.take_notice(),
            None,
            "Started retires the buffering notice"
        );
    }

    #[test]
    fn stream_pure_bypasses_the_eq_for_remote_tracks_only() {
        let (mut player, _cmds, events, params) = stream_player(JellyfinMode::StreamPure);
        // An EQ that is on and very much not flat.
        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [6, 12, 6, 0, 0, 0, 0, 0, 0, 0, 0],
        });
        assert!(
            !params.eq_bypass.load(Ordering::Relaxed),
            "default: no bypass"
        );

        // Remote track: bypassed (DSP side *and* the bit-perfect marker —
        // lossless source, full volume, centered balance, hot EQ).
        player.handle(PlayerCommand::PlayTrack(0));
        assert!(
            params.eq_bypass.load(Ordering::Relaxed),
            "stream_pure bypasses while a remote track plays"
        );
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert!(
            player.snapshot().format.bit_perfect,
            "bypassed EQ → nothing but the decoder is in the path"
        );
        assert_eq!(
            player.take_notice().as_deref(),
            Some("stream pure: eq bypassed"),
            "said once, when the first pure stream starts"
        );

        // …and the local track after it gets the EQ right back.
        player.handle(PlayerCommand::PlayTrack(1));
        assert!(
            !params.eq_bypass.load(Ordering::Relaxed),
            "local track: bypass lifts"
        );
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert!(
            !player.snapshot().format.bit_perfect,
            "hot EQ shapes local tracks again"
        );
        assert_eq!(
            player.take_notice(),
            None,
            "the stream-pure notice is a once-per-session thing"
        );

        // Back to a remote track: bypassed again, but no second notice.
        player.handle(PlayerCommand::PlayTrack(0));
        assert!(params.eq_bypass.load(Ordering::Relaxed));
        queue(&events, fake_started_at(0));
        player.tick(Duration::ZERO);
        assert_eq!(player.take_notice(), None, "noticed once per session");

        // In `stream` mode (not pure) the EQ keeps working remotely.
        let (mut player, _cmds, events, params) = stream_player(JellyfinMode::Stream);
        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [0; 11],
        });
        player.handle(PlayerCommand::PlayTrack(0));
        assert!(
            !params.eq_bypass.load(Ordering::Relaxed),
            "plain streaming keeps the EQ"
        );
        let mut hot = fake_started();
        hot.codec = "FLAC".into();
        hot.lossless = true;
        hot.bit_depth = Some(16);
        queue(&events, EngineEvent::Started(hot, fake_handle(0)));
        player.handle(PlayerCommand::SetEq {
            enabled: true,
            gains: [12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        });
        player.tick(Duration::ZERO);
        assert!(
            !player.snapshot().format.bit_perfect,
            "EQ on and not flat: still processing while streaming"
        );
    }
}
