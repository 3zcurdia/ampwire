# Plan 04 — Live timer, seeking and FFT spectrum in sync with the audio

Goal: the LCD clock, seek bar and spectrum bars follow what is *actually being
heard*. Pausing freezes them, stopping zeroes them, seeking jumps the clock.

Prerequisites: plan 03 merged (`Engine`, `RealPlayer`, `Decoder`).

## Decisions

| Topic | Decision |
|---|---|
| Clock source | Count frames the audio callback really delivered (not wall time), so it cannot drift or run while buffering. |
| Seek | `SeekBy(±5 s)` from the existing keys. Implemented by reloading the same track with a start offset (drop stream, new ring buffer, new stream). Simple and race-free; a short glitch on seek is accepted. |
| Spectrum | 2048-point FFT (Hann window) on the samples that were just played, mixed down to mono. **32 log-spaced bands**, 40 Hz → min(16 kHz, 0.45 × sample rate). Level mapped from −60 dB…0 dB to 0.0…1.0. |
| Smoothing | Same feel as the mock: instant attack, release `bin = target + (bin - target) * 0.45`; peak caps fall with factor `0.06` (copy the constants from `MockPlayer::tick`). Paused → bars decay `× 0.75` per frame (like `decay_spectrum`). Stopped → zero. |
| FFT where | On the UI thread inside `RealPlayer::tick` (≈ 30 µs per frame). Only the sample *tap* lives in the audio callback. |
| Crates | `rustfft` (FFT). `rtrb` already added. |

## Files

`Cargo.toml`, `src/audio/engine.rs`, `src/audio/decoder.rs`, `src/player/real.rs`,
`src/player/mod.rs`. New: `src/audio/analysis.rs`.

## Steps

### 1. Position counter
In the engine's `start()`: create `played: Arc<AtomicU64>` (frames, not samples) and give a clone to the
audio callback. After filling `data`, add `frames_actually_popped` (count slots where `pop()` returned `Some`, divided by `channels`).
Do **not** count padded silence. Pausing writes silence without popping, so the counter stops by itself.

Expose it: `EngineEvent::Started(info, handle)` where `handle: PlaybackHandle { played: Arc<AtomicU64>, sample_rate: u32, start_offset: Duration, tap: Consumer<f32> }`.
`RealPlayer` stores the handle. Position = `start_offset + Duration::from_secs_f64(played / sample_rate)`.
`snapshot().position` uses it; no handle (stopped) → `ZERO`. Clamp to the track duration when known.

### 2. Sample tap for the spectrum
Create a second `rtrb::RingBuffer::<f32>::new(16384)`. In the callback, after writing each output frame,
push **one mono sample** (`(L + R) / 2`, or the single channel) with `tap_producer.push(x).ok()` — if the ring is full, drop the sample (never block, never allocate).
Only push samples that were really popped (skip padding silence). The `Consumer` end goes to `RealPlayer` through `PlaybackHandle`.

### 3. Duration
`StreamInfo.duration` (from the decoder, accurate) overrides `Track.duration` when the tag duration was `None` or differs by more than 1 s.
Add `Playlist::set_duration(index, Duration)` (uses `Arc::make_mut`) and call it from `RealPlayer` on `Started`.
Remaining-time display (`ui.time_remaining`) then works for tracks that had no tag duration.

### 4. Seeking
- `Decoder::open_at(path, start: Duration)`: after probing, call
  `format.seek(SeekMode::Accurate, SeekTo::Time { time: Time::from(start.as_secs_f64()), track_id: Some(track_id) })`
  then `decoder.reset()`. The returned `SeekedTo.actual_ts` converted with the time base is the real `start_offset`
  (use that, not the requested value). If seek fails (some VBR mp3s) return `Err("seek not supported")`; the player then
  restarts the same track from 0 and sets the notice `seek not supported for this file`.
- `EngineCmd::Load { path, start: Duration }` (use `ZERO` for normal loads).
- `RealPlayer::handle(SeekBy(d))`: only when Playing or Paused. `target = (position + d).max(0)`. If the duration is known,
  `target = target.min(duration - 1s)` (so seeking past the end never skips the track). Send `Load { path, start: target }`;
  if the player was Paused, send `Pause` right after so it stays paused (the engine's pause flag must survive a `Load`:
  keep `paused` outside the per-track state).
- Update the Prev rule now that position exists: `Prev` with `position > 3 s` → seek to 0 (reload with `start: ZERO`); otherwise previous track.
  Delete the `// plan 04` comment from plan 03.

### 5. Analyzer (`src/audio/analysis.rs`) — pure, unit-testable
```rust
pub const BANDS: usize = 32;
pub struct Analyzer { /* fft plan, window[2048], ring of last 2048 samples, edges, scratch */ }
impl Analyzer {
    pub fn new() -> Self;
    /// Feed any number of new mono samples (oldest first).
    pub fn push(&mut self, samples: &[f32]);
    /// Compute band levels 0.0–1.0 for the given sample rate into `out[..BANDS]`.
    pub fn bands(&mut self, sample_rate: u32, out: &mut [f32; BANDS]);
}
```
Algorithm for `bands`:
1. Copy the last 2048 samples (zero-padded if fewer have arrived), multiply by the Hann window, run the FFT (`rustfft::FftPlanner`, build the plan once in `new`).
2. Magnitude of bin `k` (0 ≤ k < 1024): `|X[k]| / (N / 4)`. (A full-scale sine then reads ≈ 1.0.)
3. Band edges: `edge[i] = 40.0 * (hi / 40.0).powf(i as f32 / BANDS as f32)` for `i in 0..=BANDS`, with `hi = min(16000, 0.45 * sample_rate)`.
   Bin width `bw = sample_rate / 2048`. Band `i` covers bins `floor(edge[i]/bw) ..= max(ceil(edge[i+1]/bw), floor(edge[i]/bw))` — take the **max** magnitude in that range
   (bass bands may contain a single bin).
4. `db = 20 * log10(mag.max(1e-6))`; `level = ((db + 60.0) / 60.0).clamp(0.0, 1.0)`.
5. Optional tilt (keeps highs visible): add `+3 dB × log2(center_freq / 1000)` before step 4, clamped to ≥ 0 for centers below 1 kHz. Put the `3.0` in a named const `TILT_DB_PER_OCTAVE`.

### 6. Smoothing + wiring in `RealPlayer::tick`
1. If Playing: drain the tap consumer completely (`while let Ok(s) = tap.pop()` into a reusable `Vec<f32>`; cap at the last 4096), `analyzer.push(&buf)`, `analyzer.bands(rate, &mut target)`.
2. If Paused: `target = zeros` and use the decay rule instead of the release rule.
3. If Stopped: zero `spectrum` and `peaks`.
4. Apply attack/release and peak-fall exactly like `MockPlayer::tick` (copy the formulas).
5. `snapshot().spectrum` = 32 values (was 24). Check `src/ui/widgets/spectrum.rs` — it must resample to any bar count; fix it if it assumes 24.

### 7. Tests
- `Analyzer`: 1 kHz full-scale sine at 44.1 kHz → the band whose range contains 1 kHz has the maximum level and it is > 0.8; its neighbours two bands away are < 0.4.
  Silence → all `0.0`. 100 Hz sine → max is in a lower band than the 1 kHz case. Also run at 48 000 and 96 000 Hz (band edges must stay below Nyquist).
- Position math as a pure function `position(start, played_frames, rate)`: unit-test, including 96 kHz.
- Seek: with the 0.5 s fixture, `Decoder::open_at(path, 250ms)` yields roughly half the samples of a full decode (±10 %). Seeking past the end → Ok with ~0 samples or a clean `Err`, never a panic.
- `RealPlayer` with the fake `AudioOut` from plan 03: `SeekBy(5)` sends `Load{start ≈ position+5s}`; seeking while Paused sends `Pause` after; `Prev` after 4 s sends `Load{start: 0}` for the same track.
- Goldens unchanged (mock only).
- Manual: play a track with a clear bass beat and confirm bars pulse with it; pause → bars fall and the clock freezes; `→` jumps 5 s; the clock matches a stopwatch over a full minute within ~0.5 s.

### 8. Docs
README status line; AGENTS.md: mention the callback-tap design and "position = frames played".

## Done when
The clock, seek bar and bars track the music; pause/stop/seek behave as above; the three cargo checks pass.

## Assumptions I made (tell me to change)
- Seek reloads the stream (brief glitch) rather than flushing in place.
- Bars are 32 log bands, −60…0 dB.
- Bars show the signal *before* volume (plan 05 keeps the tap ahead of the gain stage).
