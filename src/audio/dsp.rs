//! Volume + balance DSP (plan 05) plus the 10-band equalizer stage (plan
//! 06): targets shared across threads, smoothing in the callback.
//!
//! The UI thread writes the target gains into [`SharedParams`] (lock-free
//! atomics); a [`Dsp`] owned by the cpal callback steps its *smoothed* gains
//! toward them once per frame and multiplies, and picks up EQ changes through
//! a version counter. When every gain target — and every smoothed gain — is
//! exactly `1.0`, the gain stage returns without touching a single sample,
//! and a flat or off EQ bypasses the EQ stage the same way (plans-README
//! rule 7: bit-perfect stays bit-perfect).
//!
//! Chain order in the callback (plan 06): decode → [`Dsp::process_eq`] →
//! spectrum tap → [`Dsp::process_gain`] → device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use super::eq::Equalizer;

/// Smoothing factor per frame: `cur += (target - cur) * SMOOTHING`. Small
/// enough to be click-free, large enough to settle in ~100 ms.
const SMOOTHING: f32 = 0.002;
/// Below this distance the smoothed gain snaps to its target (exponential
/// approaches never reach it otherwise, and the bit-perfect skip needs an
/// exact `1.0`). The plan said 1e-5, but in f32 an approach toward 1.0
/// *stalls* at ≈1.5e-5: the step `(1 - cur) * 0.002` drops below half an
/// ULP and the addition rounds to a no-op — just above the threshold, so
/// the snap (and with it the bit-perfect fast path) would never fire.
/// 1e-4 fires first; the jump is 0.01 % of full scale (≈ −0.0009 dB).
const SNAP: f32 = 1e-4;

/// Targets shared UI thread → audio callback, all lock-free (`f32::to_bits`
/// in an `AtomicU32`). Created once by the `RealPlayer`, cloned (`Arc`) into
/// the engine and its callback. Plan 06 adds the EQ fields here.
#[derive(Debug)]
pub struct SharedParams {
    pub volume_gain: AtomicU32,
    pub balance_left: AtomicU32,
    pub balance_right: AtomicU32,
    /// Route the channels to the opposite ears (`[audio] swap_channels`):
    /// for DACs/headsets whose L/R wiring is reversed. Applied *after* the
    /// gains, so it fixes the stereo image and the balance direction even
    /// at unity — it reorders samples, it never changes their values.
    pub swap_channels: AtomicBool,
    /// EQ on/off (plan 06). Flat gains bypass regardless.
    pub eq_enabled: AtomicBool,
    /// Preamp + 10 bands in whole dB (`[i8; 11]` widened to `AtomicI32`).
    pub eq_gains: [AtomicI32; 11],
    /// Writer bumps this **after** writing gains/enabled; the callback
    /// recomputes its coefficients when it sees a value it hasn't applied.
    pub eq_version: AtomicU32,
}

impl SharedParams {
    /// Unity by default: a fresh session is bit-perfect until told otherwise
    /// (the EQ starts flat, and `Dsp` treats version 0 as already applied).
    pub fn new() -> Self {
        let unity = 1.0f32.to_bits();
        Self {
            volume_gain: AtomicU32::new(unity),
            balance_left: AtomicU32::new(unity),
            balance_right: AtomicU32::new(unity),
            swap_channels: AtomicBool::new(false),
            eq_enabled: AtomicBool::new(true),
            eq_gains: std::array::from_fn(|_| AtomicI32::new(0)),
            eq_version: AtomicU32::new(0),
        }
    }

    /// Write all three gain targets at once (UI thread). Any `Relaxed`
    /// store is picked up by the next callback; a torn combination
    /// self-heals on the following frame.
    pub fn set(&self, volume_gain: f32, balance_left: f32, balance_right: f32) {
        self.volume_gain
            .store(volume_gain.to_bits(), Ordering::Relaxed);
        self.balance_left
            .store(balance_left.to_bits(), Ordering::Relaxed);
        self.balance_right
            .store(balance_right.to_bits(), Ordering::Relaxed);
    }

    /// Turn the L/R swap on or off (read once per `Dsp::process_gain` call).
    pub fn set_swap_channels(&self, swap: bool) {
        self.swap_channels.store(swap, Ordering::Relaxed);
    }

    /// Write the EQ state (UI thread): gains, then enabled, then bump the
    /// version with `Release` so the callback's `Acquire` load sees all of
    /// it. `gains[0]` is the preamp, `gains[1..=10]` the bands, in dB.
    pub fn set_eq(&self, enabled: bool, gains: [i8; 11]) {
        for (slot, &gain) in self.eq_gains.iter().zip(gains.iter()) {
            slot.store(i32::from(gain), Ordering::Relaxed);
        }
        self.eq_enabled.store(enabled, Ordering::Relaxed);
        self.eq_version.fetch_add(1, Ordering::Release);
    }
}

impl Default for SharedParams {
    fn default() -> Self {
        Self::new()
    }
}

/// The gain stage inside the cpal callback: interleaved `f32` in place,
/// no allocation, no locking (plans-README rule 6).
pub struct Dsp {
    params: Arc<SharedParams>,
    channels: usize,
    /// Smoothed gains, one step per frame toward the shared targets.
    cur_vol: f32,
    cur_l: f32,
    cur_r: f32,
    /// The EQ stage (plan 06): preamp + 10 biquads per channel.
    eq: Equalizer,
    /// Whether the EQ currently shapes audio: enabled *and* not flat.
    eq_on: bool,
    /// Last [`SharedParams::eq_version`] applied to `eq`.
    seen_version: u32,
}

impl Dsp {
    pub fn new(params: Arc<SharedParams>, sample_rate: u32, channels: usize) -> Self {
        Self {
            params,
            channels,
            cur_vol: 1.0,
            cur_l: 1.0,
            cur_r: 1.0,
            eq: Equalizer::new(sample_rate, channels),
            eq_on: false,
            seen_version: 0,
        }
    }

    /// Apply the equalizer to `samples` (interleaved, `channels` wide).
    ///
    /// Notices a bumped [`SharedParams::eq_version`] first (reads the gains,
    /// recomputes the coefficients — nothing allocated), then bypasses
    /// immediately when the EQ is off or flat: a flat or disabled EQ leaves
    /// the buffer **bit-identical**. Runs **before** the spectrum tap, so
    /// the bars show what is actually heard.
    pub fn process_eq(&mut self, samples: &mut [f32]) {
        let version = self.params.eq_version.load(Ordering::Acquire);
        if version != self.seen_version {
            let mut gains = [0i8; 11];
            for (slot, gain) in self.params.eq_gains.iter().zip(&mut gains) {
                *gain = slot.load(Ordering::Relaxed).clamp(-12, 12) as i8;
            }
            let enabled = self.params.eq_enabled.load(Ordering::Relaxed);
            self.eq.set_gains(gains);
            self.eq_on = enabled && gains.iter().any(|g| *g != 0);
            self.seen_version = version;
        }
        if self.eq_on {
            self.eq.process(samples);
        }
    }

    /// Apply volume and balance to `samples` (interleaved, `channels` wide).
    ///
    /// - stereo: volume on both, balance attenuates only the *opposite*
    ///   channel (`b > 0` lowers left, `b < 0` lowers right) — by **content**
    ///   position — then `swap_channels` (if on) exchanges the pair, so a
    ///   reversed-wired DAC still hears the intended image and balance,
    /// - any other channel count: volume only (balance and swap ignored).
    ///
    /// Call this **after** the spectrum tap and only over the frames that
    /// were really popped — never over the callback's padding silence.
    pub fn process_gain(&mut self, samples: &mut [f32]) {
        let target_vol = f32::from_bits(self.params.volume_gain.load(Ordering::Relaxed));
        let target_l = f32::from_bits(self.params.balance_left.load(Ordering::Relaxed));
        let target_r = f32::from_bits(self.params.balance_right.load(Ordering::Relaxed));
        let swap = self.params.swap_channels.load(Ordering::Relaxed);

        // Bit-perfect fast path: nothing targeted, nothing in flight and no
        // reordering → the samples pass through completely untouched. A swap
        // on its own does *not* qualify: it reorders (and therefore must
        // still run), but it never changes a sample's value.
        let unity = target_vol == 1.0
            && target_l == 1.0
            && target_r == 1.0
            && self.cur_vol == 1.0
            && self.cur_l == 1.0
            && self.cur_r == 1.0;
        if unity && !swap {
            return;
        }

        let channels = self.channels.max(1);
        let stereo = channels == 2;
        for frame in samples.chunks_mut(channels) {
            step(&mut self.cur_vol, target_vol);
            step(&mut self.cur_l, target_l);
            step(&mut self.cur_r, target_r);
            if stereo {
                // A trailing partial frame (odd length) is left alone.
                if let [left, right] = frame {
                    if !unity {
                        *left *= self.cur_vol * self.cur_l;
                        *right *= self.cur_vol * self.cur_r;
                    }
                    // Gains ran on content first; the swap routes the pair
                    // to reversed wiring *after* them (balance stays right).
                    if swap {
                        std::mem::swap(left, right);
                    }
                }
            } else if !unity {
                for sample in frame {
                    *sample *= self.cur_vol;
                }
            }
        }
    }
}

/// One smoothing step toward `target`, snapping when close enough.
fn step(cur: &mut f32, target: f32) {
    *cur += (target - *cur) * SMOOTHING;
    if (target - *cur).abs() < SNAP {
        *cur = target;
    }
}

/// Volume 0–=100 → linear gain: `(v / 100)^2`. `0` is silence, `100` is
/// exactly `1.0` (unity, so a fresh install is bit-perfect).
pub fn volume_to_gain(v: u8) -> f32 {
    let v = v.min(100) as f32 / 100.0;
    v * v
}

/// Balance −100 (full left) … +100 (full right) → `(left, right)` gains.
/// Only the opposite channel is attenuated; the other stays exactly `1.0`.
pub fn balance_to_gains(b: i8) -> (f32, f32) {
    let b = b.clamp(-100, 100) as f32;
    let left = if b <= 0.0 { 1.0 } else { 1.0 - b / 100.0 };
    let right = if b >= 0.0 { 1.0 } else { 1.0 + b / 100.0 };
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dsp(channels: usize) -> (Dsp, Arc<SharedParams>) {
        let params = Arc::new(SharedParams::new());
        (Dsp::new(Arc::clone(&params), 44_100, channels), params)
    }

    /// Run one callback pass (EQ, then gain) over `buffer`, `frames` times.
    fn run(dsp: &mut Dsp, buffer: &mut [f32], frames: usize) {
        for _ in 0..frames {
            dsp.process_eq(buffer);
            dsp.process_gain(buffer);
        }
    }

    // --- volume_to_gain (plan §7) ---

    #[test]
    fn volume_to_gain_shapes() {
        assert_eq!(volume_to_gain(0), 0.0);
        assert_eq!(volume_to_gain(100), 1.0, "unity must be exact");
        assert!((volume_to_gain(50) - 0.25).abs() < 1e-6);
        // Out-of-range input clamps instead of producing >1 gain.
        assert_eq!(volume_to_gain(200), 1.0);
    }

    #[test]
    fn volume_to_gain_is_monotonic() {
        let mut prev = -1.0f32;
        for v in 0..=100 {
            let gain = volume_to_gain(v);
            assert!(gain >= prev, "gain dipped at {v}");
            prev = gain;
        }
    }

    // --- balance_to_gains (plan §7) ---

    #[test]
    fn balance_to_gains_shapes() {
        assert_eq!(balance_to_gains(0), (1.0, 1.0));
        assert_eq!(balance_to_gains(-100), (1.0, 0.0));
        assert_eq!(balance_to_gains(100), (0.0, 1.0));
        assert_eq!(balance_to_gains(-50), (1.0, 0.5));
        assert_eq!(balance_to_gains(50), (0.5, 1.0));
        // Out-of-range input clamps, never amplifies.
        assert_eq!(balance_to_gains(127), (0.0, 1.0));
        assert_eq!(balance_to_gains(-128), (1.0, 0.0));
    }

    // --- Dsp::process (plan §7) ---

    #[test]
    fn unity_targets_leave_the_buffer_bit_identical() {
        let (mut dsp, _params) = dsp(2);
        let buffer: Vec<f32> = (0..256)
            .map(|i| (i as f32 * 0.013 - 1.0).sin() * 0.9)
            .collect();
        let mut processed = buffer.clone();
        run(&mut dsp, &mut processed, 10);
        assert_eq!(processed, buffer, "not a multiply by 1.0 — untouched");
    }

    #[test]
    fn volume_settles_at_the_target_and_never_overshoots() {
        let (mut dsp, params) = dsp(2);
        params.set(0.5, 1.0, 1.0); // a 0.5 gain target (not a volume setting)
        let mut buffer = vec![1.0f32; 512];
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for _ in 0..2000 {
            buffer.fill(1.0); // fresh input: read the gain off, not a product
            dsp.process_gain(&mut buffer);
            for frame in buffer.as_chunks::<2>().0 {
                min = min.min(frame[0]);
                max = max.max(frame[0]);
            }
        }
        assert!(
            (buffer[0] - 0.5).abs() < 1e-4,
            "settles at the 0.5 target: {}",
            buffer[0]
        );
        assert!(min >= 0.5 && max <= 1.0, "never overshoots: {min}..={max}");
    }

    #[test]
    fn balance_right_silences_left_and_leaves_right_untouched() {
        let (mut dsp, params) = dsp(2);
        let (left, right) = balance_to_gains(100);
        params.set(1.0, left, right);
        let mut buffer = vec![0.75f32; 512];
        for _ in 0..4000 {
            buffer.fill(0.75);
            dsp.process_gain(&mut buffer);
        }
        for frame in buffer.as_chunks::<2>().0 {
            assert_eq!(frame[0], 0.0, "left fully attenuated");
            assert_eq!(frame[1], 0.75, "right untouched (gain exactly 1.0)");
        }
    }

    #[test]
    fn mono_ignores_balance_entirely() {
        let (mut dsp, params) = dsp(1);
        params.set(1.0, 0.0, 1.0); // full-right balance
        let mut buffer = vec![0.5f32; 64];
        for _ in 0..100 {
            buffer.fill(0.5);
            dsp.process_gain(&mut buffer);
        }
        assert!(
            buffer.iter().all(|s| *s == 0.5),
            "balance must not touch mono"
        );
    }

    #[test]
    fn a_change_after_settling_returns_to_bit_perfect() {
        // Dip the volume, bring it back to 100 → the fast path re-engages and
        // samples are again bit-identical.
        let (mut dsp, params) = dsp(2);
        params.set(volume_to_gain(30), 1.0, 1.0);
        let mut buffer = vec![0.42f32; 256];
        for _ in 0..5000 {
            buffer.fill(0.42);
            dsp.process_gain(&mut buffer);
        }
        assert!((buffer[0] - 0.42).abs() > 1e-6, "was attenuated");
        params.set(1.0, 1.0, 1.0);
        for _ in 0..5000 {
            buffer.fill(0.42);
            dsp.process_gain(&mut buffer);
        }
        assert_eq!(buffer[0], 0.42, "unity again: samples untouched");
    }

    #[test]
    fn a_fresh_dsp_and_params_start_at_unity() {
        let (mut dsp, params) = dsp(2);
        assert_eq!(params.volume_gain.load(Ordering::Relaxed), 1.0f32.to_bits());
        let mut buffer = vec![0.9f32; 8];
        run(&mut dsp, &mut buffer, 100);
        assert_eq!(buffer, vec![0.9f32; 8]);
    }

    // --- channel swap for reversed-wiring devices ---

    #[test]
    fn swap_reorders_pairs_even_at_unity_without_changing_values() {
        let (mut dsp, params) = dsp(2);
        params.set_swap_channels(true);
        let mut buffer = vec![0.25, -0.75, 0.5, 1.0];
        // One pass = one callback: each pair is swapped exactly once (in a
        // real session the samples then move on and never come back).
        dsp.process_gain(&mut buffer);
        assert_eq!(
            buffer,
            vec![-0.75, 0.25, 1.0, 0.5],
            "reordered, values untouched"
        );
        // Turning it back off restores the untouched fast path.
        params.set_swap_channels(false);
        dsp.process_gain(&mut buffer);
        assert_eq!(buffer, vec![-0.75, 0.25, 1.0, 0.5], "swap off: alone");
    }

    #[test]
    fn swap_applies_after_the_gains_so_balance_stays_correct() {
        // Content: left 1.0, right 0.5. Balance fully left → gains (1, 0),
        // so the right content is silenced *before* the reorder; a
        // reversed-wiring DAC then hears [0.0, 1.0] on the wire, i.e. the
        // loud content on the ear's left. Swap-before-gains would instead
        // silence the left content and pan the wrong way.
        let (mut dsp, params) = dsp(2);
        let (left, right) = balance_to_gains(-100);
        params.set(1.0, left, right);
        let mut scratch = vec![0.0f32; 2048];
        for _ in 0..4000 {
            scratch.fill(0.0);
            dsp.process_gain(&mut scratch); // settle the smoothing (swap still off)
        }
        params.set_swap_channels(true);
        let mut buffer = vec![1.0, 0.5, 1.0, 0.5];
        dsp.process_gain(&mut buffer); // the one pass that carries the samples
        assert_eq!(
            buffer,
            vec![0.0, 1.0, 0.0, 1.0],
            "gains first (right silenced), swap second"
        );
    }

    #[test]
    fn swap_is_ignored_for_non_stereo_streams() {
        for channels in [1usize, 6] {
            let (mut dsp, params) = dsp(channels);
            params.set_swap_channels(true);
            let mut buffer = vec![0.5f32; channels * 4];
            run(&mut dsp, &mut buffer, 10);
            assert!(
                buffer.iter().all(|s| *s == 0.5),
                "{channels}ch: no reordering without a stereo pair"
            );
        }
    }

    // --- EQ stage (plan 06, step 2 + 6) ---

    #[test]
    fn a_flat_enabled_eq_leaves_the_buffer_bit_identical() {
        let (mut dsp, params) = dsp(2);
        params.set_eq(true, [0; 11]); // enabled, all gains 0
        let buffer: Vec<f32> = (0..1024)
            .map(|i| (i as f32 * 0.013 - 1.0).sin() * 0.9)
            .collect();
        let mut processed = buffer.clone();
        dsp.process_eq(&mut processed);
        assert_eq!(processed, buffer, "flat EQ must not touch a sample");
    }

    #[test]
    fn a_disabled_eq_with_non_zero_gains_is_also_bit_identical() {
        let (mut dsp, params) = dsp(2);
        params.set_eq(false, [6, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let buffer: Vec<f32> = (0..1024)
            .map(|i| (i as f32 * 0.013 - 1.0).sin() * 0.5)
            .collect();
        let mut processed = buffer.clone();
        dsp.process_eq(&mut processed);
        assert_eq!(processed, buffer, "EQ off: samples untouched");
    }

    #[test]
    fn a_version_bump_turns_the_eq_on_and_off_live() {
        // Mono, so the 1 kHz sine is also a 1 kHz per-channel signal.
        let (mut dsp, params) = dsp(1);
        let sine: Vec<f32> = (0..8192)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44_100.0).sin() as f32 * 0.2)
            .collect();

        // Boost the 1 kHz band: the callback notices on the next pass.
        params.set_eq(true, [0, 0, 0, 0, 0, 12, 0, 0, 0, 0, 0]);
        let mut boosted = sine.clone();
        dsp.process_eq(&mut boosted);
        let peak = boosted.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.2 * 3.98).abs() < 0.2 * 3.98 * 0.15,
            "×3.98 at 1 kHz, got peak {peak}"
        );

        // Back to flat: the very next pass is bit-identical again.
        params.set_eq(true, [0; 11]);
        let mut flat = sine.clone();
        dsp.process_eq(&mut flat);
        assert_eq!(flat, sine, "flat again after the bump");
    }

    #[test]
    fn an_active_eq_is_independent_of_the_gain_stage() {
        // EQ runs even at unity volume, and the gain stage runs even with a
        // flat EQ — each with its own bypass.
        let (mut dsp, params) = dsp(2);
        params.set_eq(true, [3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // preamp +3
        let mut buffer = vec![0.25f32; 256];
        dsp.process_eq(&mut buffer);
        assert!(
            (buffer[0] - 0.25f32 * 10f32.powf(3.0 / 20.0)).abs() < 1e-3,
            "preamp applied at unity volume"
        );
        // …and the gain stage untouched it (unity targets → early return).
        let eqd = buffer.clone();
        dsp.process_gain(&mut buffer);
        assert_eq!(buffer, eqd, "unity gain stage bypasses");
    }
}
