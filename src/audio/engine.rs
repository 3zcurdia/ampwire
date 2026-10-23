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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use super::dsp::{Dsp, SharedParams};
use super::{Decoder, StreamInfo};

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
    Pause,
    Resume,
    Stop,
    Shutdown,
}

/// Everything the UI needs from a live stream to read the clock and draw the
/// spectrum (plan 04): frames really played, and a mono sample tap.
#[derive(Debug)]
pub struct PlaybackHandle {
    /// Frames the audio callback really delivered — *not* wall time, so the
    /// clock cannot drift and stops by itself while buffering or paused.
    pub played: Arc<AtomicU64>,
    pub sample_rate: u32,
    /// Where the decoder actually landed after a seek (packet-rounded).
    pub start_offset: Duration,
    /// Consumer end of the mono sample tap: one sample per played frame.
    pub tap: Consumer<f32>,
}

/// Events from the engine thread back to the UI thread.
#[derive(Debug)]
pub enum EngineEvent {
    /// The track opened and playback started: format info for the hi-fi
    /// line, plus the handle that makes position and spectrum readable.
    Started(StreamInfo, PlaybackHandle),
    /// The decoder hit EOF **and** the ring buffer drained: auto-advance.
    Finished,
    /// The track could not be played (bad file, unsupported device config…).
    Error(String),
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

impl Engine {
    /// Spawn the engine thread. The device itself is opened lazily, on the
    /// first `Load` — spawning is safe on machines with no audio hardware.
    /// `params` are the shared volume/balance targets the callback reads
    /// (plan 05); the engine never writes them.
    pub fn spawn(params: Arc<SharedParams>) -> Engine {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let join = std::thread::Builder::new()
            .name("ampwire-engine".into())
            .spawn(move || run(cmd_rx, ev_tx, params))
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

/// A live track: stream + decoder + ring-buffer producer + EOF bookkeeping.
struct Playing {
    /// Kept alive (and dropped) on this thread — macOS `Stream` is `!Send`.
    _stream: cpal::Stream,
    decoder: Decoder,
    producer: Producer<f32>,
    /// `rtrb` capacity, so "the ring drained" can be tested exactly.
    capacity: usize,
    eof: bool,
    /// Decoded samples not yet pushed (a chunk can outgrow the free slots).
    pending: Vec<f32>,
    pending_pos: usize,
    /// Set by cpal's error callback; checked (and cleared) by the loop.
    device_error: Arc<AtomicBool>,
}

impl Playing {
    /// Push pending samples, then decode more when the pending chunk is fully
    /// in the ring. Returns the decoder's error, if any.
    fn feed(&mut self) -> Result<(), String> {
        // 1. Push as much of the pending chunk as fits right now.
        if self.pending_pos < self.pending.len() {
            let slots = self.producer.slots();
            let rest = &self.pending[self.pending_pos..];
            let (pushed, _) = self
                .producer
                .push_partial_slice(&rest[..rest.len().min(slots)]);
            self.pending_pos += pushed.len();
        }
        // 2. Refill when drained and the ring still has meaningful room.
        if self.pending_pos >= self.pending.len() && !self.eof {
            self.pending.clear();
            self.pending_pos = 0;
            if self.producer.slots() >= MIN_CHUNK {
                match self.decoder.read(&mut self.pending) {
                    Ok(true) => {}
                    Ok(false) => self.eof = true,
                    Err(reason) => return Err(reason),
                }
            }
        }
        Ok(())
    }

    /// Decoder at EOF, everything pushed, ring fully drained → `Finished`.
    fn drained(&self) -> bool {
        self.eof && self.pending_pos >= self.pending.len() && self.producer.slots() == self.capacity
    }
}

/// The engine thread loop: commands → feed the ring → sleep.
fn run(cmd_rx: Receiver<EngineCmd>, ev_tx: Sender<EngineEvent>, params: Arc<SharedParams>) {
    let mut state: Option<Playing> = None;
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
                    // so no stale audio survives into the next track.
                    state = None;
                    match start_track(&path, start, &paused, &params) {
                        Ok((playing, handle)) => {
                            let _ = ev_tx
                                .send(EngineEvent::Started(playing.decoder.info().clone(), handle));
                            state = Some(playing);
                        }
                        Err(reason) => {
                            let _ = ev_tx.send(EngineEvent::Error(reason));
                        }
                    }
                }
                EngineCmd::Pause => paused.store(true, Ordering::Relaxed),
                EngineCmd::Resume => paused.store(false, Ordering::Relaxed),
                EngineCmd::Stop => state = None,
                EngineCmd::Shutdown => {
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            break;
        }

        // 2. Surface a device failure (set from cpal's error callback).
        let device_failed = state
            .as_ref()
            .is_some_and(|playing| playing.device_error.swap(false, Ordering::Relaxed));
        if device_failed {
            let _ = ev_tx.send(EngineEvent::Error("audio device error".into()));
            state = None;
            continue;
        }

        // 3. Feed the ring buffer / notice the end of the track.
        let mut feed_error = None;
        let mut finished = false;
        if let Some(playing) = &mut state {
            match playing.feed() {
                Ok(()) => finished = playing.drained(),
                Err(reason) => feed_error = Some(reason),
            }
        }
        if let Some(reason) = feed_error {
            let _ = ev_tx.send(EngineEvent::Error(reason));
            state = None;
            continue;
        }
        if finished {
            let _ = ev_tx.send(EngineEvent::Finished);
            state = None;
        }

        // 4. Never busy-spin.
        std::thread::sleep(FEED_INTERVAL);
    }
}

/// Open a track end-to-end: decode it (seeking first when `start` is
/// non-zero), find a device config it fits, pre-fill the ring (so playback
/// starts on sound, not silence), then `play()`. Returns the live state plus
/// the handle that lets the UI read the clock and tap samples.
fn start_track(
    path: &Path,
    start: Duration,
    paused: &Arc<AtomicBool>,
    params: &Arc<SharedParams>,
) -> Result<(Playing, PlaybackHandle), String> {
    let decoder = Decoder::open_at(path, start)?;
    let info = decoder.info().clone();

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_string())?;
    let config = pick_config(&device, &info)?;

    // ≈0.5 s of the track's *own* rate and channel count: no resampling.
    let capacity = (info.sample_rate as usize * info.channels as usize / RING_SECONDS_DIVISOR)
        .max(MIN_CHUNK * 2);
    let (producer, mut consumer) = RingBuffer::<f32>::new(capacity);

    // The spectrum tap: one mono sample per played frame, dropped (never
    // blocked on) when the UI falls behind.
    let (tap_producer, tap_consumer) = RingBuffer::<f32>::new(TAP_CAPACITY);
    let mut tap_producer = tap_producer;

    // Frames the callback really delivered: the clock source (plan 04).
    let played = Arc::new(AtomicU64::new(0));
    let callback_played = Arc::clone(&played);

    let device_error = Arc::new(AtomicBool::new(false));
    let callback_error = Arc::clone(&device_error);
    let callback_paused = Arc::clone(paused);
    let channels = info.channels as usize;
    // The gain stage lives in the callback (one `Dsp` per stream); the UI
    // changes targets through the shared `params`, never through commands.
    let mut dsp = Dsp::new(Arc::clone(params), channels);
    let stream = device
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
        .map_err(|e| format!("audio device error: {e}"))?;

    let mut playing = Playing {
        _stream: stream,
        decoder,
        producer,
        capacity,
        eof: false,
        pending: Vec::new(),
        pending_pos: 0,
        device_error,
    };

    // Pre-fill to ~50 % *before* play() so the callback starts on samples.
    loop {
        if playing.eof || playing.producer.slots() <= playing.capacity / 2 {
            break;
        }
        if playing.producer.slots() < MIN_CHUNK {
            break; // belt and braces: never spin below the feed threshold
        }
        playing.feed()?;
    }

    playing
        ._stream
        .play()
        .map_err(|e| format!("audio device error: {e}"))?;

    let handle = PlaybackHandle {
        played,
        sample_rate: info.sample_rate,
        start_offset: playing.decoder.start_offset(),
        tap: tap_consumer,
    };
    Ok((playing, handle))
}

/// cpal's audio callback: atoms + ring buffers only (plans-README rule 6).
///
/// Counts the frames it *really* popped — padded silence is not counted, and
/// pausing writes silence without popping, so the counter stops by itself.
/// The mono spectrum tap is taken **before** the gain stage (plan 04's tap
/// must stay pre-DSP), and [`Dsp::process`] then runs over exactly the
/// popped frames — never over the padding silence.
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
    let mut frames_popped: u64 = 0;
    for frame in data.chunks_mut(channels) {
        let mut sum = 0.0;
        let mut popped = 0;
        for sample in frame.iter_mut() {
            match consumer.pop() {
                Ok(value) => {
                    *sample = value;
                    sum += value;
                    popped += 1;
                }
                // Ring drained: pad with silence, count nothing.
                Err(_) => *sample = 0.0,
            }
        }
        if popped == channels {
            frames_popped += 1;
            // Mono tap for the spectrum: (L + R) / 2 (or the single channel).
            // A full ring drops the sample — the callback never blocks.
            let _ = tap.push(sum / channels as f32);
        }
    }
    played.fetch_add(frames_popped, Ordering::Relaxed);
    let filled = frames_popped as usize * channels;
    dsp.process(&mut data[..filled]);
}

/// The one output config this track can play through unchanged: right channel
/// count, `f32` samples, and a rate range that contains the track's own.
/// Anything else makes the track unplayable (resampling is a later plan).
fn pick_config(device: &cpal::Device, info: &StreamInfo) -> Result<StreamConfig, String> {
    let rate = info.sample_rate;
    let channels = info.channels;
    let ranges = device
        .supported_output_configs()
        .map_err(|e| format!("audio device error: {e}"))?;
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
    Err(format!("device cannot play {rate} Hz {channels}ch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_engine_shuts_the_thread_down_promptly() {
        // No device is opened until a `Load`, so this is safe anywhere.
        let engine = Engine::spawn(Arc::new(SharedParams::new()));
        let started = std::time::Instant::now();
        drop(engine); // sends Shutdown and joins
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "`q` must never wait on the audio thread"
        );
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
        let engine = Engine::spawn(Arc::new(SharedParams::new()));
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
                    EngineEvent::Error(reason) => outcome = Some(reason),
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(engine);

        let outcome = outcome.expect("the 0.5 s tone must finish within 10 s");
        assert_eq!(outcome, "Finished");
        assert!(started, "Started must arrive before Finished");
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

    /// A unity DSP (fresh params): the gain stage leaves samples alone.
    fn unity_dsp(channels: usize) -> Dsp {
        Dsp::new(Arc::new(SharedParams::new()), channels)
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
        let mut dsp = Dsp::new(Arc::clone(&params), 2);

        // Settle the smoothing first, then run the real callback once.
        let mut scratch = [1.0f32; 2048];
        for _ in 0..2000 {
            dsp.process(&mut scratch);
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
}
