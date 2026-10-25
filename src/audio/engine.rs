//! The engine thread: owns the cpal `Stream` *and* the decoder.
//!
//! One thread owns both because on macOS the cpal `Stream` is `!Send`: it must
//! be created, used and dropped on a single thread. The UI thread talks to it
//! through two [`std::sync::mpsc`] channels ([`EngineCmd`] in, [`EngineEvent`]
//! out) and a lock-free [`rtrb`] ring buffer feeds cpal's audio callback.
//!
//! Sample path: `Decoder::read` → interleaved `f32` → SPSC ring → callback.
//! The callback only ever pops samples (atomics, no allocation, no locking —
//! plans-README rule 6); everything else happens on the engine thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::jellyfin::cache::{DIR_PREFIX as JELLYFIN_DIR_PREFIX, create_own_dir};
use crate::jellyfin::http_source::{HttpRangeSource, OpenError, download_fallback};
use crate::jellyfin::{Client, ItemId};

use super::dsp::{Dsp, SharedParams};
use super::{Decoder, PlayError, StreamInfo};

/// How often the engine thread wakes to feed the ring buffer / poll commands.
/// Never busy-spins, never sleeps long enough to starve the callback.
const FEED_INTERVAL: Duration = Duration::from_millis(5);
/// Smallest decode batch: ~11 ms of 44.1 kHz stereo, so pushes stay chunky.
const MIN_CHUNK: usize = 1024;
/// Ring capacity in samples: the plan's ≈0.5 s (`rate * channels / 2`), with a
/// floor so the `MIN_CHUNK` feed threshold can never deadlock a small ring.
const RING_SECONDS_DIVISOR: usize = 2;
/// Mono sample-tap capacity: ≈0.37 s at 44.1 kHz. The UI drains it every
/// frame; when it overflows the excess is dropped (never blocks the callback).
const TAP_CAPACITY: usize = 16384;

/// Commands from the UI thread to the engine thread.
#[derive(Debug, Clone)]
pub enum EngineCmd {
    Load {
        path: PathBuf,
        /// Start offset; `Duration::ZERO` for a plain load, `≈ position ± 5 s`
        /// for a seek (the engine reloads the stream at that point).
        start: Duration,
    },
    /// Play a Jellyfin item straight from the server (plan 08, step 6): the
    /// engine builds the seekable [`HttpRangeSource`] itself, so the
    /// credentials (the [`Client`]) live here and never in a `Track`.
    LoadRemote {
        item_id: ItemId,
        /// Container name from the library page, as the probe hint.
        container: Option<String>,
        start: Duration,
    },
    /// Pre-open the track that plays after the current one (plan 09): at
    /// EOF its samples continue in the **same** stream — no silence, no
    /// device reopen — when its format matches. Any `Load`, `LoadRemote` or
    /// `Stop` drops the queued decoder; a new `Queue` replaces it.
    Queue {
        source: QueueSource,
        /// Where the queued track starts. Always `Duration::ZERO` today (a
        /// gapless track starts at its first sample); kept for symmetry
        /// with `Load`.
        start: Duration,
        /// The queued track's `stream_pure` EQ bypass: the engine flips it
        /// at the boundary because it owns the switch moment (plan 09).
        eq_bypass: bool,
    },
    /// Drop the queued next track without touching the current one.
    Unqueue,
    Pause,
    Resume,
    Stop,
    Shutdown,
}

/// What the engine should pre-open for the *next* track (plan 09). The UI
/// builds it from `PlayOrder::upcoming(1)` — the engine never guesses what
/// comes next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueSource {
    /// A local file (also a cache-mode Jellyfin track once downloaded).
    Local(PathBuf),
    /// A `stream*`-mode Jellyfin item: the engine builds the seekable
    /// [`HttpRangeSource`] itself, credentials included (plan 08 rule).
    Remote {
        item_id: ItemId,
        container: Option<String>,
    },
}

/// Everything the UI needs from a live stream to read the clock and draw the
/// spectrum (plan 04): frames really played, and a mono sample tap.
#[derive(Debug)]
pub struct PlaybackHandle {
    /// Frames the audio callback really delivered — *not* wall time, so the
    /// clock cannot drift and stops by itself while buffering or paused.
    pub played: Arc<AtomicU64>,
    /// Frames-played value at which a queued next track's first sample is
    /// heard (plan 09). [`NO_BOUNDARY`] until a gapless switch really
    /// records one; the UI moves the track cursor only when `played`
    /// crosses it — never when [`EngineEvent::Advanced`] arrives (the event
    /// can precede the audio by the ring's length).
    pub boundary: Arc<AtomicU64>,
    pub sample_rate: u32,
    /// Where the decoder actually landed after a seek (packet-rounded).
    pub start_offset: Duration,
    /// Consumer end of the mono sample tap: one sample per played frame.
    pub tap: Consumer<f32>,
}

/// [`PlaybackHandle::boundary`] before any gapless switch: no next track
/// ever started inside this stream.
pub const NO_BOUNDARY: u64 = u64::MAX;

/// Events from the engine thread back to the UI thread.
#[derive(Debug)]
pub enum EngineEvent {
    /// The track opened and playback started: format info for the hi-fi
    /// line, plus the handle that makes position and spectrum readable.
    Started(StreamInfo, PlaybackHandle),
    /// A compatible queued track was swapped in mid-stream (plan 09): the
    /// `Stream`, the ring and the `Dsp` all carried on, only the decoder
    /// changed. The UI performs the track change when the clock crosses
    /// `PlaybackHandle::boundary`, which the engine stored *before* sending
    /// this event.
    Advanced(StreamInfo),
    /// The decoder hit EOF **and** the ring buffer drained: auto-advance.
    Finished,
    /// The track could not be played (bad file, unsupported device config,
    /// a seek the container refuses…).
    Error(PlayError),
    /// A remote track is being probed/opened — show `buffering…` *before*
    /// the blocking round trip (plan 08, step 6).
    Buffering,
    /// A one-off engine-side note (the `stream` range-fallback notice).
    Notice(String),
}

/// The player's seam over the engine, so state-machine tests can run without
/// an audio device: a fake implements this trait, records the commands and
/// replays events on [`AudioOut::poll`].
pub trait AudioOut: Send {
    fn send(&self, cmd: EngineCmd);
    fn poll(&self) -> Vec<EngineEvent>;
}

/// Handle to the engine thread. Dropping sends `Shutdown` and joins, so `q`
/// exits promptly even mid-playback.
pub struct Engine {
    tx: Sender<EngineCmd>,
    rx: Receiver<EngineEvent>,
    join: Option<JoinHandle<()>>,
}

/// What the engine needs for `mode = "stream"` / `"stream_pure"` (plan 08,
/// step 6). Present ⇔ a stream mode: remote loads, the ≈2 s ring, and the
/// no-`Range` fallback directory.
pub struct StreamSetup {
    /// Builds each [`HttpRangeSource`] — the credentials stay here, never
    /// in a `Track`.
    pub client: Client,
    /// Parent of the fallback's `ampwire-jellyfin-<pid>` (`[jellyfin]
    /// cache_dir`; the system temp dir when `None`). Created on the first
    /// fallback only, removed when the engine thread exits.
    pub fallback_parent: Option<PathBuf>,
}

impl Engine {
    /// Spawn the engine thread. The device itself is opened lazily, on the
    /// first `Load` — spawning is safe on machines with no audio hardware.
    /// `params` are the shared volume/balance targets the callback reads
    /// (plan 05); the engine never writes them. `stream` is `Some` in the
    /// `stream*` modes only.
    pub fn spawn(params: Arc<SharedParams>, stream: Option<StreamSetup>) -> Engine {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let join = std::thread::Builder::new()
            .name("ampwire-engine".into())
            .spawn(move || run(cmd_rx, ev_tx, params, stream))
            .expect("spawn the audio engine thread");
        Engine {
            tx: cmd_tx,
            rx: ev_rx,
            join: Some(join),
        }
    }
}

impl AudioOut for Engine {
    fn send(&self, cmd: EngineCmd) {
        // The engine may already be gone during shutdown; nothing to do then.
        let _ = self.tx.send(cmd);
    }

    fn poll(&self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(EngineCmd::Shutdown);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

/// A live track: the decode side (plan 09's [`Feeder`]) plus what only the
/// engine thread may touch — the `!Send` cpal `Stream` and the device-error
/// flag.
struct Playing {
    /// Kept alive (and dropped) on this thread — macOS `Stream` is `!Send`.
    _stream: cpal::Stream,
    feeder: Feeder,
    /// Set by cpal's error callback; checked (and cleared) by the loop.
    device_error: Arc<AtomicBool>,
}

/// The decode side of a live track: decoder + ring-buffer producer + EOF
/// and gapless bookkeeping (plan 09). Split out of the old `Playing` so the
/// track switch is unit-testable on any machine — no cpal device involved.
struct Feeder {
    decoder: Decoder,
    /// The pre-opened next track (plan 09, step 2), swapped in at EOF.
    queued: Option<QueuedTrack>,
    /// A queued track that failed to open. Held, not sent: it surfaces as
    /// `Error` at the boundary — when the ring drains — so the skip path
    /// is exactly today's (plan §Decisions).
    queue_error: Option<PlayError>,
    producer: Producer<f32>,
    /// `rtrb` capacity, so "the ring drained" can be tested exactly.
    capacity: usize,
    eof: bool,
    /// Decoded samples not yet pushed (a chunk can outgrow the free slots).
    pending: Vec<f32>,
    pending_pos: usize,
    /// Samples pushed into the ring since the stream started: the switch
    /// boundary, in frames, is this at the moment the decoders swap.
    pushed: u64,
    /// Frames-played value at which the queued track's first sample lands
    /// (shared with the UI through [`PlaybackHandle`]). [`NO_BOUNDARY`]
    /// until a switch really happens.
    boundary: Arc<AtomicU64>,
    /// `Some` for one `feed` after a gapless switch: the new current
    /// track's info, to announce as [`EngineEvent::Advanced`].
    switched: Option<StreamInfo>,
    /// The current track's `stream_pure` bypass, carried across the switch
    /// and applied by the loop — the engine owns the boundary, so it owns
    /// the flip (plan 09, step 6).
    eq_bypass: bool,
}

/// A next track, pre-opened (probe only — nothing decoded yet) and its
/// `stream_pure` bypass flag (plan 09).
struct QueuedTrack {
    decoder: Decoder,
    eq_bypass: bool,
}

impl Feeder {
    /// Push pending samples, then decode more when the pending chunk is fully
    /// in the ring. At the current decoder's EOF the queued one is swapped
    /// in and fed **in the same call** (plan 09, step 3) — the ring never
    /// sees the boundary. Returns the decoder's error, if any.
    fn feed(&mut self) -> Result<(), String> {
        // 1. Push as much of the pending chunk as fits right now.
        if self.pending_pos < self.pending.len() {
            let slots = self.producer.slots();
            let rest = &self.pending[self.pending_pos..];
            let (pushed, _) = self
                .producer
                .push_partial_slice(&rest[..rest.len().min(slots)]);
            self.pending_pos += pushed.len();
            self.pushed += pushed.len() as u64;
        }
        // 2. Refill when drained and the ring still has meaningful room.
        if self.pending_pos >= self.pending.len() && !self.eof && self.producer.slots() >= MIN_CHUNK
        {
            self.pending.clear();
            self.pending_pos = 0;
            let mut may_switch = true;
            loop {
                match self.decoder.read(&mut self.pending) {
                    Ok(true) => break,
                    // End of this track: continue with the queued one right
                    // away, so its first sample follows the last directly.
                    Ok(false) if may_switch && self.switch_next() => may_switch = false,
                    Ok(false) => {
                        self.eof = true;
                        break;
                    }
                    Err(reason) => return Err(reason),
                }
            }
        }
        Ok(())
    }

    /// The current decoder hit EOF: swap the queued one in — same ring,
    /// same `Dsp` (EQ state included), no silence — and record the
    /// boundary in *frames played* at the exact swap moment. Returns false
    /// when nothing compatible is queued; the caller then keeps today's
    /// EOF → drain → [`EngineEvent::Finished`] behaviour.
    fn switch_next(&mut self) -> bool {
        let Some(next) = self.queued.take() else {
            return false;
        };
        self.decoder = next.decoder;
        self.eq_bypass = next.eq_bypass;
        self.eof = false;
        // Everything pushed is eventually played, so "samples pushed so
        // far" is the frames-played value at which the new track starts.
        let channels = self.decoder.info().channels.max(1) as u64;
        self.boundary
            .store(self.pushed / channels, Ordering::Release);
        self.switched = Some(self.decoder.info().clone());
        true
    }

    /// Pre-open a next track into this stream — kept only when its format
    /// can share the ring (same sample rate and channel count, plan §Scope;
    /// a mismatch keeps today's reopen path and is not an error). Replaces
    /// any held open error.
    fn queue(&mut self, track: QueuedTrack) {
        let current = self.decoder.info();
        let next = track.decoder.info();
        if next.sample_rate == current.sample_rate && next.channels == current.channels {
            self.queued = Some(track);
        } else {
            self.queued = None;
        }
        self.queue_error = None;
    }

    /// A queued track failed to open: held for the boundary (the drain),
    /// where it surfaces as `Error` and the skip path runs unchanged.
    fn queue_failed(&mut self, reason: PlayError) {
        self.queued = None;
        self.queue_error = Some(reason);
    }

    /// Forget the queued next track entirely (`Unqueue`, and every `Load`/
    /// `Stop` by dropping the whole `Playing`).
    fn drop_queue(&mut self) {
        self.queued = None;
        self.queue_error = None;
    }

    /// Decoder at EOF, everything pushed, ring fully drained → `Finished`.
    fn drained(&self) -> bool {
        self.eof && self.pending_pos >= self.pending.len() && self.producer.slots() == self.capacity
    }
}

/// Lazily-queried, invalidatable per-session data (plan 09, step 1). `T` is
/// the output device + its advertised configs in production; the caching
/// rule itself is generic so it can be tested without hardware.
struct Cached<T> {
    value: Option<T>,
    /// How many times the (slow) query really ran — the seam the test
    /// checks: two track starts must query once.
    queries: usize,
}

impl<T> Cached<T> {
    fn new() -> Self {
        Self {
            value: None,
            queries: 0,
        }
    }

    /// The cached value, querying it on first use — and again after every
    /// [`Cached::invalidate`].
    fn get_or_query<E>(&mut self, query: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        if self.value.is_none() {
            self.value = Some(query()?);
            self.queries += 1;
        }
        Ok(self.value.as_ref().expect("cache filled above"))
    }

    /// The next [`Cached::get_or_query`] runs the query again: on a device
    /// error or a config miss — the default device may have changed (a
    /// headset plugged in a moment ago).
    fn invalidate(&mut self) {
        self.value = None;
    }
}

/// What the engine keeps about the output device between tracks (plan 09,
/// step 1): the device itself, its display name and the configs it
/// advertises — so even a non-gapless reopen skips the slow queries.
struct DeviceData {
    device: cpal::Device,
    label: String,
    ranges: Vec<cpal::SupportedStreamConfigRange>,
}

/// Query the default output device and everything a track start needs from
/// it. Runs once per session (or once per invalidation).
fn query_device() -> Result<DeviceData, PlayError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_string())?;
    // Name the device in every failure below: "CX31993 ...: cannot play
    // 96000 Hz 2ch" tells the user *why* silence happened (e.g. a headset
    // jack that only advertises 44.1/48 kHz fed from a 96 kHz file).
    let label = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "default output".into());
    let ranges = device
        .supported_output_configs()
        .map_err(|e| format!("stream error: {e}"))?
        .collect();
    Ok(DeviceData {
        device,
        label,
        ranges,
    })
}

/// A no-`Range` fallback download running on its own thread (audit P2):
/// the engine keeps serving commands meanwhile, and anything that replaces
/// the track — `Load`, `LoadRemote`, `Stop`, `Shutdown` — sets `cancel` and
/// drops it, so a late result is never played.
struct PendingDownload {
    done: Receiver<Result<PathBuf, String>>,
    cancel: Arc<AtomicBool>,
    start: Duration,
}

impl Drop for PendingDownload {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The engine thread loop: commands → feed the ring → sleep.
fn run(
    cmd_rx: Receiver<EngineCmd>,
    ev_tx: Sender<EngineEvent>,
    params: Arc<SharedParams>,
    stream: Option<StreamSetup>,
) {
    let mut state: Option<Playing> = None;
    let mut download: Option<PendingDownload> = None;
    // The fallback's own `ampwire-jellyfin-<pid>`, created on first use.
    let mut fallback_dir: Option<PathBuf> = None;
    // The output device + its advertised configs, queried once and kept
    // across tracks (plan 09, step 1).
    let mut devices = Cached::<DeviceData>::new();
    // Pause flag shared with the callback. It deliberately lives *outside*
    // the per-track state and survives a `Load`, so a seek-while-paused
    // (Load + Pause) never audibly blips: whoever wants playback re-sends
    // Resume (or the player re-sends Pause right after the seek Load).
    let paused = Arc::new(AtomicBool::new(false));

    loop {
        // 1. Commands (non-blocking).
        let mut shutdown = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCmd::Load { path, start } => {
                    // Drop the old stream first: its ring buffer dies with it,
                    // so no stale audio survives into the next track (and with
                    // it the queued next track — plan 09).
                    state = None;
                    download = None;
                    let ctx = StartCtx {
                        paused: &paused,
                        params: &params,
                        devices: &mut devices,
                        streaming: stream.is_some(),
                    };
                    match start_track(&path, start, ctx) {
                        Ok((playing, handle)) => {
                            let _ =
                                ev_tx.send(EngineEvent::Started(playing.info().clone(), handle));
                            state = Some(playing);
                        }
                        Err(reason) => {
                            let _ = ev_tx.send(EngineEvent::Error(reason));
                        }
                    }
                }
                EngineCmd::LoadRemote {
                    item_id,
                    container,
                    start,
                } => {
                    state = None;
                    download = None;
                    // The probe + open blocks ≈ one round trip: tell the UI
                    // to show `buffering…` first (plan 08, step 6).
                    let _ = ev_tx.send(EngineEvent::Buffering);
                    let remote = RemoteLoad {
                        item_id: &item_id,
                        container: container.as_deref(),
                        start,
                    };
                    let ctx = StartCtx {
                        paused: &paused,
                        params: &params,
                        devices: &mut devices,
                        streaming: stream.is_some(),
                    };
                    match open_remote(stream.as_ref(), &mut fallback_dir, remote, ctx, &ev_tx) {
                        Ok(Opened::Playing(opened)) => {
                            let (playing, handle) = *opened;
                            let _ =
                                ev_tx.send(EngineEvent::Started(playing.info().clone(), handle));
                            state = Some(playing);
                        }
                        Ok(Opened::Downloading(pending)) => download = Some(pending),
                        Err(reason) => {
                            let _ = ev_tx.send(EngineEvent::Error(reason));
                        }
                    }
                }
                EngineCmd::Queue {
                    source,
                    start,
                    eq_bypass,
                } => {
                    // Pre-open the next track (plan 09, step 2) only on a
                    // live stream; a queue while stopped is meaningless and
                    // silently dropped.
                    if let Some(playing) = &mut state {
                        match open_queued(&source, start, eq_bypass, stream.as_ref()) {
                            Ok(Some(queued)) => playing.feeder.queue(queued),
                            // Not gapless-capable (e.g. a server without
                            // `Range`): queue nothing and let the drain take
                            // the normal path.
                            Ok(None) => playing.feeder.drop_queue(),
                            Err(reason) => playing.feeder.queue_failed(reason),
                        }
                    }
                }
                EngineCmd::Unqueue => {
                    if let Some(playing) = &mut state {
                        playing.feeder.drop_queue();
                    }
                }
                EngineCmd::Pause => paused.store(true, Ordering::Relaxed),
                EngineCmd::Resume => paused.store(false, Ordering::Relaxed),
                EngineCmd::Stop => {
                    // The queued next track goes with the stream (plan 09).
                    state = None;
                    download = None;
                }
                EngineCmd::Shutdown => {
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            break;
        }

        // A fallback download finished: play its file like any local one.
        let downloaded = download
            .as_ref()
            .and_then(|pending| match pending.done.try_recv() {
                Ok(result) => Some(result.map(|path| (path, pending.start))),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("jellyfin: download failed".into())),
            });
        if let Some(result) = downloaded {
            download = None;
            let ctx = StartCtx {
                paused: &paused,
                params: &params,
                devices: &mut devices,
                streaming: stream.is_some(),
            };
            match result
                .map_err(PlayError::Failed)
                .and_then(|(path, start)| start_track(&path, start, ctx))
            {
                Ok((playing, handle)) => {
                    let _ = ev_tx.send(EngineEvent::Started(playing.info().clone(), handle));
                    state = Some(playing);
                }
                Err(reason) => {
                    let _ = ev_tx.send(EngineEvent::Error(reason));
                }
            }
        }

        // 2. Surface a device failure (set from cpal's error callback) and
        //    re-query the device on the next track start (plan 09, step 1).
        let device_failed = state
            .as_ref()
            .is_some_and(|playing| playing.device_error.swap(false, Ordering::Relaxed));
        if device_failed {
            devices.invalidate();
            let _ = ev_tx.send(EngineEvent::Error("audio device error".into()));
            state = None;
            continue;
        }

        // 3. Feed the ring buffer / notice the end of the track.
        let mut feed_error = None;
        let mut finished = false;
        let mut advanced = None;
        let mut boundary_bypass = None;
        if let Some(playing) = &mut state {
            match playing.feeder.feed() {
                Ok(()) => {
                    finished = playing.feeder.drained();
                    if let Some(info) = playing.feeder.switched.take() {
                        boundary_bypass = Some(playing.feeder.eq_bypass);
                        advanced = Some(info);
                    }
                }
                Err(reason) => feed_error = Some(reason),
            }
        }
        if let Some(bypass) = boundary_bypass {
            // The engine owns the switch moment, so it owns the
            // `stream_pure` bypass flip too (plan 09, step 6): the queued
            // track's flag lands on the shared params at the boundary.
            params.set_eq_bypass(bypass);
        }
        if let Some(info) = advanced {
            let _ = ev_tx.send(EngineEvent::Advanced(info));
        }
        if let Some(reason) = feed_error {
            let _ = ev_tx.send(EngineEvent::Error(PlayError::Failed(reason)));
            state = None;
            continue;
        }
        if finished {
            // A queued track that failed to open surfaces here — at the
            // boundary, as today's `Error`, so the skip path is unchanged
            // (plan 09 §Decisions).
            let held = state
                .as_mut()
                .and_then(|playing| playing.feeder.queue_error.take());
            match held {
                Some(reason) => {
                    let _ = ev_tx.send(EngineEvent::Error(reason));
                    state = None;
                }
                None => {
                    let _ = ev_tx.send(EngineEvent::Finished);
                    state = None;
                }
            }
        }

        // 4. Never busy-spin.
        std::thread::sleep(FEED_INTERVAL);
    }

    // Exit: cancel a running download, then remove the fallback directory
    // (the cache's own `Drop` rule, for the stream modes — audit S6).
    drop(download);
    if let Some(dir) = fallback_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
}

impl Playing {
    /// The live track's stream info (the current decoder's — after a
    /// gapless switch that is the *next* track's, plan 09).
    fn info(&self) -> &StreamInfo {
        self.feeder.decoder.info()
    }
}

/// The per-session pieces every track start needs (engine thread only):
/// the pause flag shared with the callback, the shared DSP targets, the
/// cached output device (plan 09), and the ring-buffer policy (streamed
/// tracks get ≈2 s, files ≈0.5 s).
struct StartCtx<'a> {
    paused: &'a Arc<AtomicBool>,
    params: &'a Arc<SharedParams>,
    devices: &'a mut Cached<DeviceData>,
    streaming: bool,
}

/// Open a track end-to-end: decode it (seeking first when `start` is
/// non-zero), find a device config it fits, pre-fill the ring (so playback
/// starts on sound, not silence), then `play()`. Returns the live state plus
/// the handle that lets the UI read the clock and tap samples.
fn start_track(
    path: &Path,
    start: Duration,
    ctx: StartCtx<'_>,
) -> Result<(Playing, PlaybackHandle), PlayError> {
    let decoder = Decoder::open_at(path, start)?;
    finish_start(decoder, ctx)
}

/// Pre-open the queued next track (plan 09, step 2): the probe (and, in
/// `stream*` modes, one HTTP round trip) happens *now*, ahead of the
/// boundary — but nothing is decoded yet.
///
/// - `Ok(None)`: not gapless-capable, queue nothing — the drain then takes
///   today's normal path (a server without `Range` downloads the track
///   after the current one ends, exactly as if no queue existed),
/// - `Err`: held by the engine and reported as `Error` at the boundary, so
///   the skip path is unchanged (plan §Decisions).
fn open_queued(
    source: &QueueSource,
    start: Duration,
    eq_bypass: bool,
    stream: Option<&StreamSetup>,
) -> Result<Option<QueuedTrack>, PlayError> {
    let decoder = match source {
        QueueSource::Local(path) => Decoder::open_at(path, start)?,
        QueueSource::Remote { item_id, container } => {
            let Some(setup) = stream else {
                return Ok(None); // no server configured: nothing to open
            };
            match HttpRangeSource::open(setup.client.clone(), item_id) {
                Ok(source) => Decoder::open_source(Box::new(source), container.as_deref(), start)?,
                Err(OpenError::NoRanges) => return Ok(None),
                Err(OpenError::Failed(reason)) => return Err(PlayError::Failed(reason)),
            }
        }
    };
    Ok(Some(QueuedTrack { decoder, eq_bypass }))
}

/// The output config the track's own format fits (no resampling — plan §Scope),
/// from the cached device query (plan 09, step 1). A config miss invalidates
/// and queries **once** more — the default device may have just changed —
/// before giving up with the device-named reason.
fn device_config(
    devices: &mut Cached<DeviceData>,
    info: &StreamInfo,
) -> Result<(StreamConfig, String), PlayError> {
    let first = devices.get_or_query(query_device)?;
    if let Ok(config) = pick_config(&first.ranges, info) {
        return Ok((config, first.label.clone()));
    }
    // A miss: re-query once before giving up (the default device may have
    // changed a moment ago — e.g. a headset just plugged in).
    devices.invalidate();
    let second = devices.get_or_query(query_device)?;
    match pick_config(&second.ranges, info) {
        Ok(config) => Ok((config, second.label.clone())),
        // The prefix names the device: "<device>: cannot play 96000 Hz 2ch".
        Err(reason) => Err(PlayError::Failed(format!("{}: {reason}", second.label))),
    }
}

/// One `LoadRemote` request, borrowed from the command.
struct RemoteLoad<'a> {
    item_id: &'a ItemId,
    container: Option<&'a str>,
    start: Duration,
}

/// What [`open_remote`] got going.
enum Opened {
    /// Boxed: a live `Playing` dwarfs the download handle.
    Playing(Box<(Playing, PlaybackHandle)>),
    /// No `Range` support: the original is downloading on its own thread.
    Downloading(PendingDownload),
}

/// Open a *remote* track (plan 08, step 6): the seekable HTTP source at
/// `start`. A server that ignores `Range` ([`OpenError::NoRanges`]) falls
/// back to downloading this one track first ("cache mode for this track"),
/// with a notice so the silence is explained — on a thread of its own, so
/// Stop/Next/quit never wait for it (audit P2).
fn open_remote(
    stream: Option<&StreamSetup>,
    fallback_dir: &mut Option<PathBuf>,
    remote: RemoteLoad<'_>,
    ctx: StartCtx<'_>,
    ev_tx: &Sender<EngineEvent>,
) -> Result<Opened, PlayError> {
    let stream = stream.ok_or_else(|| "jellyfin: no server configured".to_string())?;
    match HttpRangeSource::open(stream.client.clone(), remote.item_id) {
        Ok(source) => {
            let decoder = Decoder::open_source(Box::new(source), remote.container, remote.start)?;
            Ok(Opened::Playing(Box::new(finish_start(decoder, ctx)?)))
        }
        Err(OpenError::Failed(reason)) => Err(PlayError::Failed(reason)),
        Err(OpenError::NoRanges) => {
            // Not seekable over HTTP: download once, then play the local
            // file (seek, EQ and tap all work as usual from here).
            let _ = ev_tx.send(EngineEvent::Notice(
                "jellyfin: server does not support ranges, downloading".into(),
            ));
            let dir = match fallback_dir {
                Some(dir) => dir.clone(),
                None => {
                    let parent = stream
                        .fallback_parent
                        .clone()
                        .unwrap_or_else(std::env::temp_dir);
                    let dir = create_own_dir(&parent, JELLYFIN_DIR_PREFIX)
                        .map_err(|_| "jellyfin: cache unavailable".to_string())?;
                    fallback_dir.insert(dir).clone()
                }
            };
            let (done_tx, done) = channel();
            let cancel = Arc::new(AtomicBool::new(false));
            let thread_cancel = Arc::clone(&cancel);
            let client = stream.client.clone();
            let item_id = remote.item_id.clone();
            let container = remote.container.map(str::to_string);
            std::thread::Builder::new()
                .name("ampwire-jellyfin-fallback".into())
                .spawn(move || {
                    let result = download_fallback(
                        &client,
                        &item_id,
                        container.as_deref(),
                        &dir,
                        &thread_cancel,
                    );
                    let _ = done_tx.send(result);
                })
                .map_err(|_| "jellyfin: download failed".to_string())?;
            Ok(Opened::Downloading(PendingDownload {
                done,
                cancel,
                start: remote.start,
            }))
        }
    }
}

/// Everything after the decoder exists: pick a device config the track's
/// own format fits, build the ring, pre-fill it (so playback starts on
/// sound, not silence) and `play()`. `streaming` doubles the ring (≈2 s) so
/// a network hiccup cannot starve the callback (plan 08, step 6).
fn finish_start(
    decoder: Decoder,
    ctx: StartCtx<'_>,
) -> Result<(Playing, PlaybackHandle), PlayError> {
    let StartCtx {
        paused,
        params,
        devices,
        streaming,
    } = ctx;
    let info = decoder.info().clone();

    // The device + its advertised configs come from the session cache
    // (plan 09, step 1): only the first track start of a session queries.
    let (config, label) = device_config(devices, &info)?;

    // ≈0.5 s of the track's *own* rate and channel count: no resampling.
    // Streamed tracks get ≈2 s (`rate * channels * 2`) so a network hiccup
    // cannot starve the callback (plan 08, step 6).
    let capacity = if streaming {
        (info.sample_rate as usize * info.channels as usize * 2).max(MIN_CHUNK * 2)
    } else {
        (info.sample_rate as usize * info.channels as usize / RING_SECONDS_DIVISOR)
            .max(MIN_CHUNK * 2)
    };
    let (producer, mut consumer) = RingBuffer::<f32>::new(capacity);

    // The spectrum tap: one mono sample per played frame, dropped (never
    // blocked on) when the UI falls behind.
    let (tap_producer, tap_consumer) = RingBuffer::<f32>::new(TAP_CAPACITY);
    let mut tap_producer = tap_producer;

    // Frames the callback really delivered: the clock source (plan 04).
    let played = Arc::new(AtomicU64::new(0));
    let callback_played = Arc::clone(&played);

    // The gapless boundary this stream may record (plan 09): `NO_BOUNDARY`
    // until a queued track really gets swapped in.
    let boundary = Arc::new(AtomicU64::new(NO_BOUNDARY));
    let feeder_boundary = Arc::clone(&boundary);

    let device_error = Arc::new(AtomicBool::new(false));
    let callback_error = Arc::clone(&device_error);
    let callback_paused = Arc::clone(paused);
    let channels = info.channels as usize;
    // The DSP stages live in the callback (one `Dsp` per stream); the UI
    // changes targets through the shared `params`, never through commands.
    let mut dsp = Dsp::new(Arc::clone(params), info.sample_rate, channels);
    // `device_config` just filled (or validated) the cache: this read is
    // free — no second `supported_output_configs()` round trip.
    let output = devices.get_or_query(query_device)?;
    let stream = output
        .device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
                fill(
                    data,
                    &mut consumer,
                    &mut tap_producer,
                    &callback_paused,
                    &callback_played,
                    channels,
                    &mut dsp,
                )
            },
            move |_err| callback_error.store(true, Ordering::Relaxed),
            None,
        )
        .map_err(|e| format!("{label}: stream error: {e}"))?;

    let feeder = Feeder {
        decoder,
        queued: None,
        queue_error: None,
        producer,
        capacity,
        eof: false,
        pending: Vec::new(),
        pending_pos: 0,
        pushed: 0,
        boundary: feeder_boundary,
        switched: None,
        eq_bypass: false,
    };
    let mut playing = Playing {
        _stream: stream,
        feeder,
        device_error,
    };

    // Pre-fill to ~50 % *before* play() so the callback starts on samples.
    loop {
        if playing.feeder.eof || playing.feeder.producer.slots() <= playing.feeder.capacity / 2 {
            break;
        }
        if playing.feeder.producer.slots() < MIN_CHUNK {
            break; // belt and braces: never spin below the feed threshold
        }
        playing.feeder.feed()?;
    }

    playing
        ._stream
        .play()
        .map_err(|e| format!("{label}: stream error: {e}"))?;

    let handle = PlaybackHandle {
        played,
        boundary,
        sample_rate: info.sample_rate,
        start_offset: playing.feeder.decoder.start_offset(),
        tap: tap_consumer,
    };
    Ok((playing, handle))
}

/// cpal's audio callback: atoms + ring buffers only (plans-README rule 6).
///
/// Counts the frames it *really* popped — padded silence is not counted, and
/// pausing writes silence without popping, so the counter stops by itself.
/// Chain order (plan 06): pop → [`Dsp::process_eq`] → the mono spectrum tap
/// → [`Dsp::process_gain`] — all three over exactly the popped frames, never
/// over the callback's padding silence.
fn fill(
    data: &mut [f32],
    consumer: &mut Consumer<f32>,
    tap: &mut Producer<f32>,
    paused: &AtomicBool,
    played: &AtomicU64,
    channels: usize,
    dsp: &mut Dsp,
) {
    if paused.load(Ordering::Relaxed) {
        data.fill(0.0);
        return;
    }
    let channels = channels.max(1);
    // Whole frames only: the engine may have pushed half a frame, and
    // popping that alone would shift every later frame by a sample. The
    // remainder waits in the ring for the rest of its frame. One bulk
    // chunk read, not an atomic round trip per sample.
    let frames_popped = (data.len() / channels).min(consumer.slots() / channels);
    let filled = frames_popped * channels;
    if let Ok(chunk) = consumer.read_chunk(filled) {
        let (first, second) = chunk.as_slices();
        data[..first.len()].copy_from_slice(first);
        data[first.len()..filled].copy_from_slice(second);
        chunk.commit_all();
    }
    // Ring drained: pad with silence, count nothing.
    data[filled..].fill(0.0);
    let frames_popped = frames_popped as u64;
    // 1. EQ first: everything downstream (tap, gains, device) hears the
    //    shaped signal.
    dsp.process_eq(&mut data[..filled]);
    // 2. Mono spectrum tap: (L + R) / 2 (or the single channel), after the
    //    EQ so the bars show what is really heard. A full ring drops the
    //    sample — the callback never blocks.
    for frame in data[..filled].chunks(channels) {
        let sum: f32 = frame.iter().sum();
        let _ = tap.push(sum / channels as f32);
    }
    // 3. Gain stage last, over exactly the popped frames.
    played.fetch_add(frames_popped, Ordering::Relaxed);
    dsp.process_gain(&mut data[..filled]);
}

/// The one output config this track can play through unchanged: right channel
/// count, `f32` samples, and a rate range that contains the track's own.
/// Anything else makes the track unplayable (resampling is a later plan).
/// `ranges` comes from the cached [`DeviceData`] query (plan 09, step 1).
fn pick_config(
    ranges: &[cpal::SupportedStreamConfigRange],
    info: &StreamInfo,
) -> Result<StreamConfig, String> {
    let rate = info.sample_rate;
    let channels = info.channels;
    for range in ranges {
        if range.channels() != channels
            || range.sample_format() != SampleFormat::F32
            || rate < range.min_sample_rate()
            || rate > range.max_sample_rate()
        {
            continue;
        }
        return Ok(range.with_sample_rate(rate).into());
    }
    // The common silence case: the device exists and is the default, but
    // advertises no config holding this exact rate/channels (no resampling
    // by design). The caller prefixes the device name.
    Err(format!("cannot play {rate} Hz {channels}ch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_engine_shuts_the_thread_down_promptly() {
        // No device is opened until a `Load`, so this is safe anywhere.
        let engine = Engine::spawn(Arc::new(SharedParams::new()), None);
        let started = std::time::Instant::now();
        drop(engine); // sends Shutdown and joins
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "`q` must never wait on the audio thread"
        );
    }

    /// Audit P2: a server that ignores `Range` makes the engine download the
    /// whole original. That must not hold the engine thread hostage — `q`
    /// (drop → Shutdown + join) returns promptly mid-download.
    #[test]
    fn quitting_during_a_fallback_download_is_prompt() {
        use crate::jellyfin::test_server::{KEY, MockServer};
        let server = MockServer::start();
        server.set_no_ranges(true);
        server.set_audio_delay(Duration::from_secs(3));
        let parent = tempfile::tempdir().expect("fallback parent");
        let engine = Engine::spawn(
            Arc::new(SharedParams::new()),
            Some(StreamSetup {
                client: server.client(KEY),
                fallback_parent: Some(parent.path().to_path_buf()),
            }),
        );
        engine.send(EngineCmd::LoadRemote {
            item_id: "slowfallback".into(),
            container: Some("flac".into()),
            start: Duration::ZERO,
        });
        // Wait until the engine has announced the fallback download.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut noticed = false;
        while !noticed && std::time::Instant::now() < deadline {
            noticed = engine
                .poll()
                .iter()
                .any(|event| matches!(event, EngineEvent::Notice(_)));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(noticed, "the range fallback started");

        let started = std::time::Instant::now();
        drop(engine);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "quit waited {:?} for the download",
            started.elapsed()
        );
        // Audit S6: the fallback directory goes with the engine.
        assert!(
            parent.path().read_dir().unwrap().next().is_none(),
            "fallback dir removed on exit"
        );
    }

    // --- plan 09: device cache + the gapless switch ---

    /// `tests/fixtures/<name>` — unit tests run from the crate root.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// Everything a fixture decodes to, standalone — the reference both
    /// halves of a gapless stream are compared against.
    fn decode_fixture(name: &str) -> Vec<f32> {
        let mut decoder = Decoder::open_at(&fixture(name), Duration::ZERO)
            .unwrap_or_else(|e| panic!("{name} must open: {e}"));
        let mut samples = Vec::new();
        while decoder
            .read(&mut samples)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
        {}
        samples
    }

    /// A [`Feeder`] over a fixture with a small ring (so a test drains it
    /// quickly) plus its consumer end — the switch needs no cpal device.
    fn feeder_of(name: &str) -> (Feeder, Consumer<f32>) {
        let decoder = Decoder::open_at(&fixture(name), Duration::ZERO)
            .unwrap_or_else(|e| panic!("{name} must open: {e}"));
        let (producer, consumer) = RingBuffer::<f32>::new(4096);
        let feeder = Feeder {
            decoder,
            queued: None,
            queue_error: None,
            producer,
            capacity: 4096,
            eof: false,
            pending: Vec::new(),
            pending_pos: 0,
            pushed: 0,
            boundary: Arc::new(AtomicU64::new(NO_BOUNDARY)),
            switched: None,
            eq_bypass: false,
        };
        (feeder, consumer)
    }

    /// Decode `name` and queue it — the plan's happy path.
    fn queued_fixture(name: &str, eq_bypass: bool) -> QueuedTrack {
        QueuedTrack {
            decoder: Decoder::open_at(&fixture(name), Duration::ZERO)
                .unwrap_or_else(|e| panic!("{name} must open: {e}")),
            eq_bypass,
        }
    }

    /// Feed and drain until the feeder reports the end of the stream,
    /// collecting every sample exactly as the callback would pop it.
    fn feed_to_end(feeder: &mut Feeder, consumer: &mut Consumer<f32>) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..10_000 {
            feeder.feed().expect("fixtures decode");
            while let Ok(sample) = consumer.pop() {
                out.push(sample);
            }
            if feeder.drained() {
                return out;
            }
        }
        panic!("the feeder must reach the drained state");
    }

    /// Plan 09, step 1: the device + config query runs once for the whole
    /// session and again only after an invalidation (device error / config
    /// miss) — so even a non-gapless reopen skips the slow queries.
    #[test]
    fn the_device_cache_queries_once_until_invalidated() {
        fn counted(queries: &mut usize, value: u32) -> Result<u32, ()> {
            *queries += 1;
            Ok(value)
        }
        let mut cache = Cached::<u32>::new();
        let mut queries = 0usize;
        assert_eq!(
            *cache.get_or_query(|| counted(&mut queries, 40)).unwrap(),
            40
        );
        assert_eq!(
            *cache.get_or_query(|| counted(&mut queries, 41)).unwrap(),
            40
        );
        assert_eq!(queries, 1, "two track starts query the configs once");
        assert_eq!(cache.queries, 1, "the engine-visible counter agrees");
        cache.invalidate();
        assert_eq!(
            *cache.get_or_query(|| counted(&mut queries, 42)).unwrap(),
            42
        );
        assert_eq!(queries, 2, "invalidation re-queries");
    }

    /// Plan 09, steps 3 + 7: at EOF the queued decoder continues in the
    /// **same** ring — A's last sample is immediately followed by B's
    /// first, with the boundary recorded in frames and the `Advanced`
    /// info carrying B's format.
    #[test]
    fn feed_switches_decoders_without_a_gap() {
        let expected_a = decode_fixture("tone.wav");
        let expected_b = decode_fixture("tone.flac");
        let (mut feeder, mut consumer) = feeder_of("tone.wav");
        feeder.queue(queued_fixture("tone.flac", true));

        let mut announced = Vec::new();
        let mut out = Vec::new();
        loop {
            feeder.feed().expect("fixtures decode");
            while let Ok(sample) = consumer.pop() {
                out.push(sample);
            }
            if let Some(info) = feeder.switched.take() {
                announced.push(info);
            }
            assert!(
                out.len() <= expected_a.len() + expected_b.len(),
                "the ring never invents samples"
            );
            if feeder.drained() {
                break;
            }
        }

        assert_eq!(announced.len(), 1, "switched exactly once");
        assert_eq!(announced[0].codec, "FLAC", "`Advanced` carries B's info");
        assert_eq!(
            out.len(),
            expected_a.len() + expected_b.len(),
            "A then B, sample-exact"
        );
        let (head, tail) = out.split_at(expected_a.len());
        assert_eq!(head, expected_a.as_slice(), "A's samples unchanged");
        assert_eq!(
            tail,
            expected_b.as_slice(),
            "B's first sample sits right after A's last — no gap, no silence"
        );
        assert_eq!(
            feeder.boundary.load(Ordering::Relaxed),
            expected_a.len() as u64 / 2,
            "the boundary is A's frame count (stereo)"
        );
        assert!(
            feeder.eq_bypass,
            "the queued track's `stream_pure` flag crossed the boundary"
        );
    }

    /// Plan 09 §Scope: a next track whose format does not fit the running
    /// stream (mono vs. stereo here) is never queued, so the drain keeps
    /// today's behaviour — `Finished`, then the UI's normal reopen.
    #[test]
    fn an_incompatible_queued_track_is_never_switched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mono = dir.path().join("mono.wav");
        write_wav_mono(&mono, 44_100, 4096); // mono ≠ the stereo stream

        let (mut feeder, mut consumer) = feeder_of("tone.wav");
        feeder.queue(QueuedTrack {
            decoder: Decoder::open_at(&mono, Duration::ZERO).expect("mono opens"),
            eq_bypass: false,
        });
        assert!(feeder.queued.is_none(), "a format mismatch is not queued");
        assert!(
            feeder.queue_error.is_none(),
            "a format change is a reopen, not an error"
        );

        feed_to_end(&mut feeder, &mut consumer);
        assert!(feeder.switched.is_none(), "no gapless switch happened");
        assert_eq!(
            feeder.boundary.load(Ordering::Relaxed),
            NO_BOUNDARY,
            "no boundary recorded"
        );
    }

    /// Plan 09 §Decisions: a queued track that cannot be opened is *held*,
    /// not sent — it surfaces as `Error` only at the boundary (the drain),
    /// so playback of the current track continues and the skip path runs
    /// exactly as it does today. A later successful queue clears it.
    #[test]
    fn a_failed_queued_open_is_held_until_the_boundary() {
        let (mut feeder, mut consumer) = feeder_of("tone.wav");
        feeder.queue(queued_fixture("tone.flac", false));
        feeder.queue_failed(PlayError::Failed("cannot decode".into()));
        assert!(
            feeder.queued.is_none(),
            "the failure replaces the queued decoder"
        );
        // A successful queue clears a previously held failure…
        feeder.queue(queued_fixture("tone.flac", false));
        assert!(feeder.queue_error.is_none(), "fresh queue clears the error");
        // …and the next failure is held until the current track drains.
        feeder.queue_failed(PlayError::Failed("no such file".into()));

        feed_to_end(&mut feeder, &mut consumer);
        assert!(
            feeder.queue_error.is_some(),
            "held at the drain — the loop then sends `Error` instead of `Finished`"
        );
        assert!(feeder.switched.is_none());
    }

    /// A 16-bit PCM WAV written by hand: no fixture ships in mono (or at
    /// another rate), and the incompatible-track test needs one.
    fn write_wav_mono(path: &Path, sample_rate: u32, frames: usize) {
        use std::io::Write as _;
        let data_len = (frames * 2) as u32;
        let mut file = std::fs::File::create(path).expect("wav create");
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap(); // byte rate
        file.write_all(&2u16.to_le_bytes()).unwrap(); // block align
        file.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
        file.write_all(b"data").unwrap();
        file.write_all(&data_len.to_le_bytes()).unwrap();
        for i in 0..frames {
            let sample = (i as f32 * 0.05).sin() * 0.5;
            file.write_all(&((sample * i16::MAX as f32) as i16).to_le_bytes())
                .unwrap();
        }
    }

    /// Plan 03, step 9 — the check that cannot be fully automated:
    /// `Load` a fixture through the *real* device and wait for `Finished`.
    /// Ignored by default: it opens the sound card (and would beep).
    ///
    /// ```sh
    /// cargo test -- --ignored   # on a machine with an output device
    /// ```
    #[test]
    #[ignore = "plays an audible tone through the real output device"]
    fn real_engine_plays_a_fixture_end_to_end() {
        let host = cpal::default_host();
        if host.default_output_device().is_none() {
            eprintln!("skipping: no audio output device on this machine");
            return;
        }

        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.wav");
        let engine = Engine::spawn(Arc::new(SharedParams::new()), None);
        engine.send(EngineCmd::Load {
            path: fixture,
            start: Duration::ZERO,
        });

        // 0.5 s of audio + drain, with generous slack for a slow device.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut started = false;
        let mut outcome = None;
        while outcome.is_none() && std::time::Instant::now() < deadline {
            for event in engine.poll() {
                match event {
                    EngineEvent::Started(info, _handle) => {
                        started = true;
                        eprintln!("started: {info:?}");
                    }
                    EngineEvent::Finished => {
                        outcome = Some("Finished".to_string());
                    }
                    EngineEvent::Error(reason) => outcome = Some(reason.to_string()),
                    EngineEvent::Buffering | EngineEvent::Notice(_) | EngineEvent::Advanced(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(engine);

        let outcome = outcome.expect("the 0.5 s tone must finish within 10 s");
        assert_eq!(outcome, "Finished");
        assert!(started, "Started must arrive before Finished");
    }

    /// Plan 09, step 7 — the device half of the same check: two
    /// same-format fixtures play back to back through **one** stream. The
    /// zero-frame continuity itself is proven by
    /// `feed_switches_decoders_without_a_gap` (the ring is the same one the
    /// callback pops); this proves the boundary really fires on hardware.
    /// Ignored by default: it opens the sound card (two audible beeps).
    ///
    /// ```sh
    /// cargo test -- --ignored   # on a machine with an output device
    /// ```
    #[test]
    #[ignore = "plays audible tones through the real output device"]
    fn two_tones_play_back_to_back_through_one_stream() {
        let host = cpal::default_host();
        if host.default_output_device().is_none() {
            eprintln!("skipping: no audio output device on this machine");
            return;
        }

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let engine = Engine::spawn(Arc::new(SharedParams::new()), None);
        engine.send(EngineCmd::Load {
            path: dir.join("tone.wav"),
            start: Duration::ZERO,
        });
        engine.send(EngineCmd::Queue {
            source: QueueSource::Local(dir.join("tone.flac")),
            start: Duration::ZERO,
            eq_bypass: false,
        });

        // 0.5 s + 0.5 s + drain, with generous slack for a slow device.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut outcome = None;
        let mut order: Vec<&'static str> = Vec::new();
        let mut boundary: Option<Arc<AtomicU64>> = None;
        while outcome.is_none() && std::time::Instant::now() < deadline {
            for event in engine.poll() {
                match event {
                    EngineEvent::Started(_info, handle) => {
                        boundary = Some(handle.boundary);
                        order.push("started");
                    }
                    EngineEvent::Advanced(info) => {
                        eprintln!("advanced: {info:?}");
                        order.push("advanced");
                    }
                    EngineEvent::Finished => {
                        order.push("finished");
                        outcome = Some("Finished".to_string());
                    }
                    EngineEvent::Error(reason) => outcome = Some(reason.to_string()),
                    EngineEvent::Buffering | EngineEvent::Notice(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(engine);

        let outcome = outcome.expect("two 0.5 s tones must finish within 15 s");
        assert_eq!(outcome, "Finished");
        assert_eq!(
            order,
            ["started", "advanced", "finished"],
            "one stream: the switch fires before the drain, never as an error"
        );
        let recorded = boundary
            .expect("Started carries the handle")
            .load(Ordering::Relaxed);
        assert_ne!(
            recorded, NO_BOUNDARY,
            "the boundary was recorded at the switch"
        );
        eprintln!("gapless boundary at {recorded} frames played");
    }

    // --- the callback: the clock source and the spectrum tap (pure) ---

    /// A stereo ring with `frames` of `(left, right)` = `(1.0, 0.0)`.
    fn stereo_ring(frames: usize) -> (Producer<f32>, Consumer<f32>) {
        let (mut producer, consumer) = RingBuffer::<f32>::new(frames * 2);
        for _ in 0..frames {
            producer.push(1.0).expect("ring has room");
            producer.push(0.0).expect("ring has room");
        }
        (producer, consumer)
    }

    /// A unity DSP (fresh params): both stages leave samples alone.
    fn unity_dsp(channels: usize) -> Dsp {
        Dsp::new(Arc::new(SharedParams::new()), 44_100, channels)
    }

    #[test]
    fn fill_counts_only_real_frames_and_taps_mono() {
        let (producer, mut consumer) = stereo_ring(3);
        let (mut tap_producer, mut tap_consumer) = RingBuffer::<f32>::new(8);
        let paused = AtomicBool::new(false);
        let played = AtomicU64::new(0);
        let mut data = [9.9f32; 8]; // 4 stereo frames requested, only 3 available
        let mut dsp = unity_dsp(2);

        fill(
            &mut data,
            &mut consumer,
            &mut tap_producer,
            &paused,
            &played,
            2,
            &mut dsp,
        );
        // 3 frames popped, the 4th padded with silence and not counted.
        assert_eq!(played.load(Ordering::Relaxed), 3);
        assert_eq!(&data[6..], &[0.0, 0.0], "drained tail is padded");
        // The tap carries one mono sample per frame: (1 + 0) / 2.
        let tapped: Vec<f32> = std::iter::from_fn(|| tap_consumer.pop().ok()).collect();
        assert_eq!(tapped, vec![0.5, 0.5, 0.5]);
        drop(producer);
    }

    #[test]
    fn fill_while_paused_writes_silence_and_freezes_the_clock() {
        let (producer, mut consumer) = stereo_ring(4);
        let (mut tap_producer, mut tap_consumer) = RingBuffer::<f32>::new(8);
        let paused = AtomicBool::new(true);
        let played = AtomicU64::new(7); // already 7 frames in
        let mut data = [9.9f32; 8];
        let mut dsp = unity_dsp(2);

        fill(
            &mut data,
            &mut consumer,
            &mut tap_producer,
            &paused,
            &played,
            2,
            &mut dsp,
        );
        assert_eq!(data, [0.0; 8], "pause writes silence");
        assert_eq!(played.load(Ordering::Relaxed), 7, "no frames counted");
        assert!(tap_consumer.pop().is_err(), "nothing tapped while paused");
        // The ring is untouched: resuming picks up exactly where it left off.
        assert_eq!(consumer.slots(), 8);
        drop(producer);
    }

    #[test]
    fn fill_applies_the_gain_stage_to_popped_frames_only() {
        // Halve the volume, then check that only the popped frames were
        // attenuated (after settling) and the padded tail stays plain silence.
        let (producer, mut consumer) = stereo_ring(2);
        let (mut tap_producer, _tap_consumer) = RingBuffer::<f32>::new(8);
        let paused = AtomicBool::new(false);
        let played = AtomicU64::new(0);
        let mut data = [1.0f32; 8]; // 4 stereo frames requested, 2 available
        let params = Arc::new(SharedParams::new());
        params.set(0.25, 1.0, 1.0); // volume 50 → gain 0.25
        let mut dsp = Dsp::new(Arc::clone(&params), 44_100, 2);

        // Settle the smoothing first, then run the real callback once.
        let mut scratch = [1.0f32; 2048];
        for _ in 0..2000 {
            dsp.process_gain(&mut scratch);
        }
        fill(
            &mut data,
            &mut consumer,
            &mut tap_producer,
            &paused,
            &played,
            2,
            &mut dsp,
        );
        assert_eq!(played.load(Ordering::Relaxed), 2);
        // The ring carries (1.0, 0.0) frames: left becomes 0.25, right 0.0.
        assert_eq!(&data[..4], &[0.25, 0.0, 0.25, 0.0], "popped: ×0.25");
        assert_eq!(&data[4..], &[0.0; 4], "padding untouched by DSP");
        drop(producer);
    }

    /// Audit P1: the engine pushes `min(pending, slots)` samples, so the
    /// ring can end mid-frame. An underrun there must leave the dangling
    /// sample in the ring — popping it alone would shift every later frame
    /// by one sample (L/R swapped until the next `Load`) and put the gain
    /// stage over the wrong range.
    #[test]
    fn an_underrun_mid_frame_never_splits_a_frame() {
        let (mut producer, mut consumer) = RingBuffer::<f32>::new(16);
        // Two whole frames (L = +n, R = −n), then frame 3's left only.
        for sample in [1.0, -1.0, 2.0, -2.0, 3.0] {
            producer.push(sample).expect("room");
        }
        let (mut tap_producer, _tap) = RingBuffer::<f32>::new(16);
        let paused = AtomicBool::new(false);
        let played = AtomicU64::new(0);
        let mut dsp = unity_dsp(2);

        let mut data = [9.9f32; 8]; // 4 frames asked, 2.5 available
        fill(
            &mut data,
            &mut consumer,
            &mut tap_producer,
            &paused,
            &played,
            2,
            &mut dsp,
        );
        assert_eq!(&data, &[1.0, -1.0, 2.0, -2.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(played.load(Ordering::Relaxed), 2);
        assert_eq!(consumer.slots(), 1, "the half frame waits in the ring");

        // The decoder catches up: frame 3's right, then frame 4.
        for sample in [-3.0, 4.0, -4.0] {
            producer.push(sample).expect("room");
        }
        let mut data = [9.9f32; 4];
        fill(
            &mut data,
            &mut consumer,
            &mut tap_producer,
            &paused,
            &played,
            2,
            &mut dsp,
        );
        assert_eq!(&data, &[3.0, -3.0, 4.0, -4.0], "channels stay aligned");
        assert_eq!(played.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn fill_taps_the_signal_after_the_eq_stage() {
        // Chain order (plan 06): EQ → tap → gains. A −6 dB preamp (×≈0.5)
        // means the tap sees the *shaped* mono sum, not the raw one.
        let (producer, mut consumer) = stereo_ring(4);
        let (mut tap_producer, mut tap_consumer) = RingBuffer::<f32>::new(8);
        let paused = AtomicBool::new(false);
        let played = AtomicU64::new(0);
        let mut data = [9.9f32; 8];
        let params = Arc::new(SharedParams::new());
        params.set_eq(true, [-6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut dsp = Dsp::new(Arc::clone(&params), 44_100, 2);

        fill(
            &mut data,
            &mut consumer,
            &mut tap_producer,
            &paused,
            &played,
            2,
            &mut dsp,
        );
        // Frames are (1.0, 0.0): post-EQ left is ≈0.499, so the mono tap is
        // half of that — ≈0.249, not the pre-EQ 0.5.
        let tapped: Vec<f32> = std::iter::from_fn(|| tap_consumer.pop().ok()).collect();
        assert_eq!(tapped.len(), 4);
        for sample in tapped {
            assert!(
                (sample - 10f32.powf(-6.0 / 20.0) / 2.0).abs() < 1e-3,
                "tap must carry the EQ-shaped signal: {sample}"
            );
        }
        drop(producer);
    }
}
