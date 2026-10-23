//! Volume + balance DSP (plan 05): targets shared across threads, smoothing
//! in the callback.
//!
//! The UI thread writes the target gains into [`SharedParams`] (three lock-free
//! atomics); a [`Dsp`] owned by the cpal callback steps its *smoothed* gains
//! toward them once per frame and multiplies. When every target — and every
//! smoothed gain — is exactly `1.0`, `process` returns without touching a
//! single sample (plans-README rule 7: bit-perfect stays bit-perfect).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

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
}

impl SharedParams {
    /// Unity by default: a fresh session is bit-perfect until told otherwise.
    pub fn new() -> Self {
        let unity = 1.0f32.to_bits();
        Self {
            volume_gain: AtomicU32::new(unity),
            balance_left: AtomicU32::new(unity),
            balance_right: AtomicU32::new(unity),
        }
    }

    /// Write all three targets at once (UI thread). Any `Relaxed` store is
    /// picked up by the next callback; a torn combination self-heals on the
    /// following frame.
    pub fn set(&self, volume_gain: f32, balance_left: f32, balance_right: f32) {
        self.volume_gain
            .store(volume_gain.to_bits(), Ordering::Relaxed);
        self.balance_left
            .store(balance_left.to_bits(), Ordering::Relaxed);
        self.balance_right
            .store(balance_right.to_bits(), Ordering::Relaxed);
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
}

impl Dsp {
    pub fn new(params: Arc<SharedParams>, channels: usize) -> Self {
        Self {
            params,
            channels,
            cur_vol: 1.0,
            cur_l: 1.0,
            cur_r: 1.0,
        }
    }

    /// Apply volume and balance to `samples` (interleaved, `channels` wide).
    ///
    /// - stereo: volume on both, balance attenuates only the *opposite*
    ///   channel (`b > 0` lowers left, `b < 0` lowers right),
    /// - any other channel count: volume only (balance is ignored).
    ///
    /// Call this **after** the spectrum tap and only over the frames that
    /// were really popped — never over the callback's padding silence.
    pub fn process(&mut self, samples: &mut [f32]) {
        let target_vol = f32::from_bits(self.params.volume_gain.load(Ordering::Relaxed));
        let target_l = f32::from_bits(self.params.balance_left.load(Ordering::Relaxed));
        let target_r = f32::from_bits(self.params.balance_right.load(Ordering::Relaxed));

        // Bit-perfect fast path: nothing targeted, nothing in flight → the
        // samples pass through completely untouched.
        let unity = target_vol == 1.0
            && target_l == 1.0
            && target_r == 1.0
            && self.cur_vol == 1.0
            && self.cur_l == 1.0
            && self.cur_r == 1.0;
        if unity {
            return;
        }

        let channels = self.channels.max(1);
        for frame in samples.chunks_mut(channels) {
            step(&mut self.cur_vol, target_vol);
            step(&mut self.cur_l, target_l);
            step(&mut self.cur_r, target_r);
            if channels == 2 {
                frame[0] *= self.cur_vol * self.cur_l;
                frame[1] *= self.cur_vol * self.cur_r;
            } else {
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
        (Dsp::new(Arc::clone(&params), channels), params)
    }

    /// Run `process` over `buffer` in callback-sized chunks, `frames` times.
    fn run(dsp: &mut Dsp, buffer: &mut [f32], frames: usize) {
        for _ in 0..frames {
            dsp.process(buffer);
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
            dsp.process(&mut buffer);
            for frame in buffer.chunks_exact(2) {
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
            dsp.process(&mut buffer);
        }
        for frame in buffer.chunks_exact(2) {
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
            dsp.process(&mut buffer);
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
            dsp.process(&mut buffer);
        }
        assert!((buffer[0] - 0.42).abs() > 1e-6, "was attenuated");
        params.set(1.0, 1.0, 1.0);
        for _ in 0..5000 {
            buffer.fill(0.42);
            dsp.process(&mut buffer);
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
}
