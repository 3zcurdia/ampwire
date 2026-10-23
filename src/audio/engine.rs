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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use super::{Decoder, StreamInfo};

/// How often the engine thread wakes to feed the ring buffer / poll commands.
/// Never busy-spins, never sleeps long enough to starve the callback.
const FEED_INTERVAL: Duration = Duration::from_millis(5);
/// Smallest decode batch: ~11 ms of 44.1 kHz stereo, so pushes stay chunky.
const MIN_CHUNK: usize = 1024;
/// Ring capacity in samples: the plan's ≈0.5 s (`rate * channels / 2`), with a
/// floor so the `MIN_CHUNK` feed threshold can never deadlock a small ring.
const RING_SECONDS_DIVISOR: usize = 2;

/// Commands from the UI thread to the engine thread.
#[derive(Debug, Clone)]
pub enum EngineCmd {
    Load { path: PathBuf },
    Pause,
    Resume,
    Stop,
    Shutdown,
}

/// Events from the engine thread back to the UI thread.
#[derive(Debug)]
pub enum EngineEvent {
    /// The track opened and playback started (format info for the hi-fi line).
    Started(StreamInfo),
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
    pub fn spawn() -> Engine {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let join = std::thread::Builder::new()
            .name("ampwire-engine".into())
            .spawn(move || run(cmd_rx, ev_tx))
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
fn run(cmd_rx: Receiver<EngineCmd>, ev_tx: Sender<EngineEvent>) {
    let mut state: Option<Playing> = None;
    let paused = Arc::new(AtomicBool::new(false));

    loop {
        // 1. Commands (non-blocking).
        let mut shutdown = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCmd::Load { path } => {
                    // Drop the old stream first: its ring buffer dies with it,
                    // so no stale audio survives into the next track.
                    state = None;
                    paused.store(false, Ordering::Relaxed);
                    match start(&path, &paused) {
                        Ok(playing) => {
                            let _ =
                                ev_tx.send(EngineEvent::Started(playing.decoder.info().clone()));
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

/// Open a track end-to-end: decode it, find a device config it fits, pre-fill
/// the ring (so playback starts on sound, not silence), then `play()`.
fn start(path: &Path, paused: &Arc<AtomicBool>) -> Result<Playing, String> {
    let decoder = Decoder::open(path)?;
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

    let device_error = Arc::new(AtomicBool::new(false));
    let callback_error = Arc::clone(&device_error);
    let callback_paused = Arc::clone(paused);
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| fill(data, &mut consumer, &callback_paused),
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
    Ok(playing)
}

/// cpal's audio callback: atoms + ring buffer only (plans-README rule 6).
fn fill(data: &mut [f32], consumer: &mut Consumer<f32>, paused: &AtomicBool) {
    if paused.load(Ordering::Relaxed) {
        data.fill(0.0);
        return;
    }
    for sample in data.iter_mut() {
        *sample = consumer.pop().unwrap_or(0.0);
    }
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
        let engine = Engine::spawn();
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
        let engine = Engine::spawn();
        engine.send(EngineCmd::Load { path: fixture });

        // 0.5 s of audio + drain, with generous slack for a slow device.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut started = false;
        let mut outcome = None;
        while outcome.is_none() && std::time::Instant::now() < deadline {
            for event in engine.poll() {
                match event {
                    EngineEvent::Started(info) => {
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
}
