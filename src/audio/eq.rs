//! The 10-band equalizer's filter math (plan 06): peaking biquads in series
//! plus a preamp gain, per the RBJ "Audio EQ Cookbook". Pure, allocation-free
//! after `new`, and testable without an audio device — the cpal callback owns
//! one [`Equalizer`] behind [`super::Dsp`].
//!
//! Precision: coefficients and per-band state are `f64`; samples are `f32`
//! only at the edges (an `f32` → `f64` → `f32` round-trip is exact).

/// Band center frequencies, Hz — same order as `BAND_LABELS[1..=10]` in
/// `src/ui/equalizer.rs`.
pub const BAND_HZ: [f64; 10] = [
    60.0, 170.0, 310.0, 600.0, 1000.0, 3000.0, 6000.0, 12000.0, 14000.0, 16000.0,
];

/// Q of every band (≈ 1 octave).
pub const Q: f64 = 1.41;

/// Gain range in whole dB (`UiState.eq_bands`).
pub const GAIN_MIN: i8 = -12;
pub const GAIN_MAX: i8 = 12;

/// A band whose center sits at or above this fraction of the sample rate is
/// skipped: near Nyquist the bilinear transform warps it into garbage (at
/// 22050 Hz that drops the 12k/14k/16k bands).
const NYQUIST_GUARD: f64 = 0.45;

/// Peaking-filter coefficients, already divided by `a0` (Direct Form II
/// Transposed). `1.0/0.0/0.0/0.0/0.0` is the identity.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    /// Pass-through.
    const fn flat() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }
}

/// RBJ cookbook peaking EQ at `f0` with `gain_db` dB of boost/cut, Q = [`Q`].
/// At `f0` the magnitude is exactly `10^(gain_db / 20)`; away from it the
/// response blends back into unity.
fn peaking(f0: f64, gain_db: f64, fs: f64) -> Coeffs {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
    let alpha = w0.sin() / (2.0 * Q);
    let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * w0.cos(), 1.0 - alpha * a);
    let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * w0.cos(), 1.0 - alpha / a);
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// One biquad stage: coefficients plus the two transposed-form state cells.
/// One per band per channel — channels never share state.
#[derive(Clone, Copy, Debug)]
struct Biquad {
    c: Coeffs,
    z1: f64,
    z2: f64,
}

impl Biquad {
    const fn flat() -> Self {
        Self {
            c: Coeffs::flat(),
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.c.b0 * x + self.z1;
        self.z1 = self.c.b1 * x - self.c.a1 * y + self.z2;
        self.z2 = self.c.b2 * x - self.c.a2 * y;
        y
    }
}

/// Preamp + 10 peaking bands, interleaved and in-place. `set_gains` keeps
/// the filter state (no clicks when a slider moves), `process` clamps its
/// output to ±1.0 after the last stage (wrap protection, not a limiter).
pub struct Equalizer {
    fs: f64,
    channels: usize,
    /// Linear preamp gain, `10^(preamp_db / 20)`.
    preamp: f64,
    /// One `[Biquad; 10]` per channel.
    filters: Vec<[Biquad; 10]>,
    /// Bands that currently do work: non-zero gain *and* a usable frequency.
    active: [bool; 10],
}

impl Equalizer {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            fs: sample_rate.max(1) as f64,
            channels,
            preamp: 1.0,
            filters: (0..channels.max(1))
                .map(|_| std::array::from_fn(|_| Biquad::flat()))
                .collect(),
            active: [false; 10],
        }
    }

    /// `gains[0]` = preamp dB, `gains[1..=10]` = bands dB. Keeps filter
    /// state (no clicks), recomputes coefficients; out-of-range values are
    /// clamped to ±12 dB. A band is marked inactive — and skipped in
    /// `process` — when its gain is 0 or `f0 >= 0.45 * fs`.
    pub fn set_gains(&mut self, gains: [i8; 11]) {
        self.preamp = 10f64.powf(gains[0].clamp(GAIN_MIN, GAIN_MAX) as f64 / 20.0);
        for (i, &f0) in BAND_HZ.iter().enumerate() {
            let db = gains[i + 1].clamp(GAIN_MIN, GAIN_MAX) as f64;
            self.active[i] = db != 0.0 && f0 < NYQUIST_GUARD * self.fs;
            let coeffs = if self.active[i] {
                peaking(f0, db, self.fs)
            } else {
                Coeffs::flat()
            };
            for channel in &mut self.filters {
                channel[i].c = coeffs;
            }
        }
    }

    /// Interleaved in-place: preamp → each active band, in order → clamp to
    /// ±1.0. A trailing partial frame is processed sample-wise (the caller
    /// only ever passes whole frames).
    pub fn process(&mut self, frames: &mut [f32]) {
        // Nothing to do: don't touch a single sample (bit-perfect).
        if self.preamp == 1.0 && !self.active.iter().any(|&a| a) {
            return;
        }
        let channels = self.channels.max(1);
        for (i, sample) in frames.iter_mut().enumerate() {
            let channel = &mut self.filters[i % channels];
            let mut x = *sample as f64 * self.preamp;
            for (band, biquad) in channel.iter_mut().enumerate() {
                if self.active[band] {
                    x = biquad.run(x);
                }
            }
            *sample = x.clamp(-1.0, 1.0) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 44100;

    fn eq(gains: [i8; 11], channels: usize) -> Equalizer {
        let mut eq = Equalizer::new(FS, channels);
        eq.set_gains(gains);
        eq
    }

    fn flat_gains() -> [i8; 11] {
        [0; 11]
    }

    /// A long sine at `freq`, run through the EQ; returns
    /// `rms(output) / rms(input)` after discarding the first 4096 samples.
    fn rms_ratio(freq: f64, gains: [i8; 11]) -> f64 {
        let mut eq = eq(gains, 1);
        let n = 65_536;
        let amplitude = 0.25;
        let mut buffer: Vec<f32> = (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * freq * i as f64 / FS as f64).sin() as f32 * amplitude
            })
            .collect();
        eq.process(&mut buffer);
        let mut out = 0.0;
        // A sine's mean square is amplitude²/2.
        let input_mean_square = amplitude as f64 * amplitude as f64 / 2.0;
        let mut input = 0.0;
        for &s in &buffer[4096..] {
            out += f64::from(s) * f64::from(s);
            input += input_mean_square;
        }
        (out / input).sqrt()
    }

    /// |H(e^jw)| of the transfer function with the given coefficients.
    fn magnitude(c: &Coeffs, w: f64) -> f64 {
        let (cw, sw) = (w.cos(), w.sin());
        let (c2, s2) = (2.0 * cw * cw - 1.0, 2.0 * sw * cw);
        let num = (c.b0 + c.b1 * cw + c.b2 * c2).hypot(-c.b1 * sw - c.b2 * s2);
        let den = (1.0 + c.a1 * cw + c.a2 * c2).hypot(-c.a1 * sw - c.a2 * s2);
        num / den
    }

    /// Cookbook check: at 0 dB the peaking filter *is* the identity.
    #[test]
    fn peaking_at_zero_db_is_identity() {
        let c = peaking(1000.0, 0.0, FS as f64);
        assert!((c.b0 - 1.0).abs() < 1e-12, "b0 = {}", c.b0);
        assert!((c.b1 - c.a1).abs() < 1e-12, "b1 = {}", c.b1);
        assert!((c.b2 - c.a2).abs() < 1e-12, "b2 = {}", c.b2);
    }

    /// The cookbook's defining property: at `f0`, |H| = 10^(dB/20).
    #[test]
    fn peaking_hits_the_target_gain_at_center() {
        for (f0, db) in [
            (60.0, 12.0),
            (1000.0, 12.0),
            (1000.0, -12.0),
            (16_000.0, 6.0),
        ] {
            let c = peaking(f0, db, FS as f64);
            let w0 = 2.0 * std::f64::consts::PI * f0 / FS as f64;
            let gain = magnitude(&c, w0);
            assert!(
                (gain - 10f64.powf(db / 20.0)).abs() < 1e-9,
                "|H({f0} Hz)| = {gain}, want {}",
                10f64.powf(db / 20.0)
            );
        }
    }

    // --- frequency response (plan §6) ---

    #[test]
    fn plus_12_db_band_boosts_1k_and_leaves_100hz_alone() {
        let mut gains = flat_gains();
        gains[5] = 12; // the 1 kHz band
        let at_1k = rms_ratio(1000.0, gains);
        assert!(
            (at_1k - 3.98).abs() / 3.98 < 0.05,
            "1 kHz sine should be ×3.98, got {at_1k}"
        );
        let at_100 = rms_ratio(100.0, gains);
        assert!(
            (at_100 - 1.0).abs() < 0.05,
            "100 Hz sine should stay ×1.0, got {at_100}"
        );
    }

    #[test]
    fn minus_12_db_band_cuts_1k_to_a_quarter() {
        let mut gains = flat_gains();
        gains[5] = -12;
        let at_1k = rms_ratio(1000.0, gains);
        assert!(
            (at_1k - 0.25).abs() / 0.25 < 0.05,
            "−12 dB at 1 kHz should be ×0.25, got {at_1k}"
        );
    }

    #[test]
    fn preamp_plus_6_doubles_the_level() {
        // Small input: +6 dB lands near ±0.7, so the ±1 clamp never fires.
        let mut eq = eq([6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 1);
        let n = 16_384;
        let mut buffer: Vec<f32> = (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 440.0 * i as f64 / FS as f64).sin() as f32 * 0.25
            })
            .collect();
        let expected: f64 = 0.25 * 10f64.powf(6.0 / 20.0) / 2.0f64.sqrt();
        eq.process(&mut buffer);
        let mut out = 0.0;
        for &s in &buffer[4096..] {
            out += f64::from(s) * f64::from(s);
        }
        let rms = (out / (n - 4096) as f64).sqrt();
        assert!(
            (rms - expected).abs() / expected < 0.05,
            "preamp +6 dB should be ×2 (rms {rms} vs {expected})"
        );
    }

    // --- safety and edge cases ---

    #[test]
    fn flat_gains_leave_the_buffer_bit_identical() {
        let mut eq = eq(flat_gains(), 2);
        let buffer: Vec<f32> = (0..512)
            .map(|i| (i as f32 * 0.013 - 1.0).sin() * 0.9)
            .collect();
        let mut processed = buffer.clone();
        eq.process(&mut processed);
        assert_eq!(processed, buffer, "flat EQ must not touch a sample");
    }

    #[test]
    fn full_scale_boost_stays_within_plus_minus_one() {
        let mut gains = flat_gains();
        for g in gains.iter_mut() {
            *g = 12;
        }
        let mut eq = eq(gains, 2);
        let n = 32_768;
        let mut buffer: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / FS as f64).sin() as f32)
            .collect();
        eq.process(&mut buffer);
        assert!(
            buffer.iter().all(|s| s.is_finite() && s.abs() <= 1.0),
            "output must be clamped to ±1.0"
        );
        // And the boost really happened somewhere below the clamp.
        assert!(
            buffer.iter().any(|s| s.abs() > 0.99),
            "a +12 dB full-scale sine should saturate sometimes"
        );
    }

    #[test]
    fn common_sample_rates_stay_finite() {
        for fs in [22050u32, 44100, 48000, 96000] {
            let mut gains = flat_gains();
            gains[0] = 6;
            for g in gains.iter_mut().skip(1) {
                *g = 12;
            }
            let mut eq = Equalizer::new(fs, 2);
            eq.set_gains(gains);
            let mut buffer: Vec<f32> = (0..4096)
                .map(|i| (i as f32 * 0.37 - 0.9).sin() * 0.8)
                .collect();
            eq.process(&mut buffer);
            assert!(
                buffer.iter().all(|s| s.is_finite()),
                "{fs} Hz produced NaN/inf"
            );
        }
    }

    #[test]
    fn bands_above_the_nyquist_guard_are_skipped_at_22050() {
        // 0.45 × 22050 = 9922.5 Hz: the 12k/14k/16k bands must go inactive
        // (their coefficients would fold past Nyquist); 6 kHz still runs.
        let mut gains = flat_gains();
        gains[7] = 12; // 6 kHz
        gains[8] = 12; // 12 kHz
        gains[9] = 12; // 14 kHz
        gains[10] = 12; // 16 kHz
        let mut eq = Equalizer::new(22050, 2);
        eq.set_gains(gains);
        assert_eq!(
            eq.active,
            [
                false, false, false, false, false, false, true, false, false, false
            ],
            "only 6 kHz survives below 0.45 × 22050"
        );
        // 12 kHz, 14 kHz, 16 kHz are skipped; everything below still runs.
        let mut buffer: Vec<f32> = (0..4096)
            .map(|i| (i as f32 * 0.61 - 0.3).sin() * 0.5)
            .collect();
        eq.process(&mut buffer);
        assert!(buffer.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn channels_stay_independent() {
        // Left-only stereo input must not leak into (or filter through) the
        // right channel's state.
        let mut gains = flat_gains();
        gains[5] = 12;
        let mut eq = eq(gains, 2);
        let frames = 8192;
        let mut buffer: Vec<f32> = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let left =
                (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / FS as f64).sin() as f32 * 0.2;
            buffer.push(left);
            buffer.push(0.0);
        }
        eq.process(&mut buffer);
        for frame in buffer.as_chunks::<2>().0 {
            assert_eq!(frame[1], 0.0, "right channel must stay silent");
            assert!(frame[0].is_finite());
        }
    }

    #[test]
    fn gains_out_of_range_clamp_to_the_slider_span() {
        // +20 dB would break the ±1 clamp's promise; set_gains clamps to ±12.
        let mut eq = eq([20, -30, 0, 0, 0, 0, 0, 0, 0, 0, 0], 1);
        let mut buffer = vec![0.25f32; 4096];
        eq.process(&mut buffer);
        assert!(buffer.iter().all(|s| s.abs() <= 1.0));
        // −30 on band 1 clamps to −12: a deep but finite cut, no silence.
        assert!(
            buffer.iter().any(|s| *s != 0.0),
            "clamped cut, not a kill switch"
        );
    }
}
