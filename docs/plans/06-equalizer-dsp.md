# Plan 06 — Real 10-band equalizer

Goal: the EQ panel sliders (preamp + 10 bands) actually shape the sound. The
`on` toggle works, settings persist, and when the EQ is off or flat the audio
passes through **untouched** (bit-perfect preserved).

Prerequisites: plan 05 merged (`Dsp`, `SharedParams`, config persistence with a debounced save).

## Decisions

| Topic | Decision |
|---|---|
| Design | 10 peaking biquad filters (RBJ "Audio EQ Cookbook") in series + preamp gain. Direct Form II Transposed. |
| Bands (Hz) | 60, 170, 310, 600, 1000, 3000, 6000, 12000, 14000, 16000 (same order as `BAND_LABELS[1..=10]` in `src/ui/equalizer.rs`). |
| Q | 1.41 for every band. |
| Gain range | −12…+12 dB per band and preamp, whole dB (matches `UiState.eq_bands: [i8; 11]`; index 0 = preamp). |
| Precision | Coefficients and filter state in `f64`; samples are `f32` at the edges. |
| Skip rule | A band is skipped if `f0 >= 0.45 * sample_rate`. |
| Bypass | EQ off, or preamp and all bands == 0 → `process` returns immediately, samples untouched. |
| Safety | When the EQ is active, hard-clamp the output to ±1.0 after the last stage (prevents wrap/garbage; it is not a limiter). |
| Order in the chain | decode → **EQ** → spectrum tap → volume/balance → device. |
| Control channel | Lock-free: atomics in `SharedParams`; the callback notices a version bump and recomputes coefficients. |
| Persistence | `[eq]` table in `config.toml`, saved with the same debounce as audio. |
| Out of scope | The `auto` toggle (per-track EQ) stays inert/greyed. Presets are plan 07. |

## Files

`src/audio/dsp.rs`, new `src/audio/eq.rs`, `src/audio/engine.rs`, `src/player/mod.rs`, `src/player/real.rs`,
`src/action.rs`, `src/keymap.rs`, `src/app.rs`, `src/config.rs`, `src/ui/equalizer.rs`, `README.md`.

## Steps

### 1. Filter math (`src/audio/eq.rs`) — pure and testable
```rust
pub const BAND_HZ: [f64; 10] = [60.0, 170.0, 310.0, 600.0, 1000.0, 3000.0, 6000.0, 12000.0, 14000.0, 16000.0];
pub const Q: f64 = 1.41;

#[derive(Clone, Copy)] struct Coeffs { b0: f64, b1: f64, b2: f64, a1: f64, a2: f64 }   // already divided by a0
fn peaking(f0: f64, gain_db: f64, fs: f64) -> Coeffs {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
    let alpha = w0.sin() / (2.0 * Q);
    let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * w0.cos(), 1.0 - alpha * a);
    let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * w0.cos(), 1.0 - alpha / a);
    Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}
struct Biquad { c: Coeffs, z1: f64, z2: f64 }      // one per band per channel
impl Biquad { fn run(&mut self, x: f64) -> f64 { let y = self.c.b0 * x + self.z1; self.z1 = self.c.b1 * x - self.c.a1 * y + self.z2; self.z2 = self.c.b2 * x - self.c.a2 * y; y } }

pub struct Equalizer { fs: f64, channels: usize, preamp: f64, filters: Vec<[Biquad; 10]> /* per channel */, active: [bool; 10] }
impl Equalizer {
    pub fn new(sample_rate: u32, channels: usize) -> Self;
    /// gains[0] = preamp dB, gains[1..=10] = bands dB. Keeps filter state (no clicks), recomputes coefficients.
    pub fn set_gains(&mut self, gains: [i8; 11]);
    /// Interleaved in-place. Applies preamp, then bands in order, then clamps to ±1.
    pub fn process(&mut self, frames: &mut [f32]);
}
```
Per frame, per channel `ch = i % channels`: `x = sample as f64 * preamp_gain`, pass through each active band's biquad of channel `ch`, then `y.clamp(-1.0, 1.0) as f32`.
`preamp_gain = 10^(preamp_db / 20)`. Bands with `gain == 0` are marked inactive (skipped) to save CPU. Verify the formulas against the cookbook; do not change them.

### 2. Shared parameters
Extend `SharedParams` (plan 05) with:
```rust
pub eq_enabled: AtomicBool,
pub eq_gains: [AtomicI32; 11],
pub eq_version: AtomicU32,        // writer bumps AFTER writing gains/enabled
```
UI side helper `SharedParams::set_eq(&self, enabled: bool, gains: [i8; 11])` — write gains, write enabled, then `eq_version.fetch_add(1, Release)`.
Callback side: in `Dsp`, keep `seen_version`, `eq: Equalizer`, `eq_on: bool`. At the top of `process`, `if params.eq_version.load(Acquire) != seen_version` → read gains + enabled, call `eq.set_gains`, compute `eq_on = enabled && gains.iter().any(|g| *g != 0)`, update `seen_version`.
(`set_gains` allocates nothing: filters are preallocated in `new`.) `Dsp::new` takes `sample_rate` and `channels`.
`Dsp::process` order: **EQ → tap → gain**. Since the tap lives in the engine's callback code (plan 04), restructure so the callback calls
`dsp.process_eq(&mut buf)`, then pushes tap samples, then `dsp.process_gain(&mut buf)`. Keep both methods, each with its own early-return bypass.

### 3. Commands and snapshot
- `PlayerCommand::SetEq { enabled: bool, gains: [i8; 11] }` (this is why `PlayerCommand` lost `Copy` in plan 02). `RealPlayer::handle` → `params.set_eq(...)` and remembers `eq_active = enabled && any != 0`.
- `RealPlayer::snapshot().format.bit_perfect` becomes `volume == 100 && balance == 0 && !eq_active`.
- `MockPlayer`: accept and ignore.

### 4. UI wiring (`src/app.rs`)
- Today `change_band` only edits `ui.eq_bands`. After every change (and after toggling `eq_on`), call `self.send_eq()`:
  `self.player.handle(PlayerCommand::SetEq { enabled: self.ui.eq_on, gains: self.ui.eq_bands })`.
- New actions (README rule 8), EQ scope: `EqToggle` (`"eq_on"`, key `"o"`, "equalizer on/off"), `EqReset` (`"eq_reset"`, key `"0"`, "reset equalizer to flat").
  `EqReset` sets all 11 values to 0. `EqToggle` flips `ui.eq_on`. Both call `send_eq()` and mark the config dirty.
- `ui.eq_auto` stays unchanged and has no action.
- Make sure the EQ panel visibly dims or marks sliders when `eq_on == false` — check `src/ui/equalizer.rs`; if it already renders the on/off state, leave it.

### 5. Persistence
`EqConfig { on: bool (default true), preamp: i8 (0), bands: [i8; 10] (all 0) }` under `Config.eq`, `#[serde(default)]`.
`sanitize`: clamp every value to −12..=12. `Config::save`: write `[eq]` like `[audio]` (bands as an integer array — write a small `int_array` helper next to `string_array`).
Startup (non-demo): copy into `ui.eq_on`, `ui.eq_bands = [preamp, bands…]`, then `send_eq()`.
Every EQ change updates `config.eq` and `config_dirty_since` (the debounce from plan 05). `--demo` keeps `DETERMINISTIC_EQ_BANDS` and never touches the config.

### 6. Tests
- `peaking` at 0 dB: `b0 == 1`, `b1 == a1`, `b2 == a2` (within 1e-12) → identity.
- Frequency response (feed a long sine, discard the first 4096 samples, measure RMS ratio) at fs = 44100:
  +12 dB band at 1 kHz: 1 kHz sine ≈ ×3.98 (±5 %); 100 Hz sine ≈ ×1.0 (±5 %). −12 dB: ≈ ×0.25. Preamp +6 → ≈ ×2 (use a small input so the ±1 clamp does not interfere).
- Bypass: all-zero gains, `enabled = true` → `process_eq` leaves the buffer **bit-identical** (`assert_eq!`). `enabled = false` with non-zero gains → also identical.
- Sample rates 44100, 48000, 96000, 22050 do not produce NaN/inf; bands above `0.45 * fs` are skipped at 22050.
- Clamp: +12 dB everywhere with a full-scale sine → every output sample within ±1.0.
- Channel independence: left-only input stays left-only after processing.
- Config round-trip and clamping; app test: `BandUp` followed by dispatch results in a `SetEq` carrying the new gains (use the fake player/Mock recorder); `EqReset` zeroes all; `o` toggles `eq_on`.
- Keymap table + conflict tests updated (`o`/`0` are Eq-scope only; confirm no clash with global bindings).
- Manual: play music, push the 60 Hz band to +12 → obvious bass; toggle `o` → A/B change; flat + on → no audible or measurable change; `bit-perfect` marker is off when EQ is active and back on after `0`.

### 7. Docs
README keys (`o`, `0` in EQ focus), `[eq]` config example; AGENTS.md DSP chain order.

## Done when
Moving a slider changes the sound within a frame or two, `o` toggles it, a flat/off EQ is bit-identical, settings survive a restart, and the three cargo checks pass.

## Assumptions I made (tell me to change)
- Whole-dB steps only; Q = 1.41 on all bands; ±1.0 hard clip as the only protection.
- `auto` (per-track EQ) is not implemented.
