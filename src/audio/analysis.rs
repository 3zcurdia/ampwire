//! The spectrum analyzer: a pure, unit-testable 2048-point FFT that turns the
//! mono sample tap (plan 04, step 2) into 32 log-spaced band levels.
//!
//! No threads, no I/O, no allocation after [`Analyzer::new`] — the UI thread
//! feeds it the samples the audio callback tapped and reads band levels back
//! (plan §"FFT where": ~30 µs per frame, cheap enough for the render loop).

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Log-spaced bands, 40 Hz → `min(16 kHz, 0.45 × sample rate)`.
pub const BANDS: usize = 32;
/// FFT length: 2048 samples ≈ 46 ms at 44.1 kHz.
const FFT_LEN: usize = 2048;
/// Bins below the half-spectrum: bin `k` covers `k × (rate / 2048)` Hz.
const BINS: usize = FFT_LEN / 2;
/// Lowest band edge in Hz.
const LO_HZ: f32 = 40.0;
/// Highest band edge cap in Hz (also bounded by 0.45 × rate, below Nyquist).
const HI_CAP_HZ: f32 = 16_000.0;
/// Keeps highs visible: `+3 dB` per octave above 1 kHz (0 below it).
const TILT_DB_PER_OCTAVE: f32 = 3.0;
/// Mapped to level 0.0: −60 dBFS is as quiet as the bars ever show.
const FLOOR_DB: f32 = 60.0;

/// 2048-point Hann-windowed FFT → 32 log-spaced band levels (0.0–1.0).
pub struct Analyzer {
    /// Built once in `new`: the FFT plan is the expensive part.
    fft: Arc<dyn Fft<f32>>,
    /// Hann window, precomputed over the whole FFT length.
    window: [f32; FFT_LEN],
    /// Ring of the last `FFT_LEN` mono samples.
    ring: [f32; FFT_LEN],
    /// Write cursor into `ring` (one past the newest sample).
    write: usize,
    /// How many samples have arrived (caps at `FFT_LEN`).
    filled: usize,
    /// In-place FFT input/output.
    spectrum: Vec<Complex<f32>>,
    /// `process_with_scratch` scratch space.
    scratch: Vec<Complex<f32>>,
    /// `BANDS + 1` band edges in Hz, cached per sample rate.
    edges: Vec<f32>,
    /// The sample rate `edges` was computed for (`0` = never).
    edge_rate: u32,
}

impl Analyzer {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        // Hann window: 0.5 × (1 − cos(2πn / N)); a full-scale sine then reads
        // ≈ 1.0 after the `|X[k]| / (N / 4)` normalization (plan §step 5.2).
        let window: [f32; FFT_LEN] = std::array::from_fn(|n| {
            0.5 * (1.0 - (2.0 * std::f32::consts::PI * n as f32 / FFT_LEN as f32).cos())
        });
        let scratch_len = fft.get_inplace_scratch_len();
        Self {
            fft,
            window,
            ring: [0.0; FFT_LEN],
            write: 0,
            filled: 0,
            spectrum: vec![Complex::new(0.0, 0.0); FFT_LEN],
            scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            edges: vec![0.0; BANDS + 1],
            edge_rate: 0,
        }
    }

    /// Feed any number of new mono samples (oldest first). Only the most
    /// recent [`FFT_LEN`] are kept; older ones are dropped.
    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            self.ring[self.write] = s;
            self.write = (self.write + 1) % FFT_LEN;
            self.filled = (self.filled + 1).min(FFT_LEN);
        }
    }

    /// Compute band levels 0.0–1.0 for the given sample rate into `out[..BANDS]`.
    pub fn bands(&mut self, sample_rate: u32, out: &mut [f32; BANDS]) {
        if sample_rate == 0 {
            out.fill(0.0);
            return;
        }
        self.update_edges(sample_rate);

        // 1. Last 2048 samples, right-aligned (newest at the end), zero-padded
        //    at the front when fewer have arrived, multiplied by the Hann
        //    window. The oldest kept sample sits `filled` slots behind the
        //    write cursor; walk forward from there, wrapping by a compare.
        let pad = FFT_LEN - self.filled;
        self.spectrum[..pad].fill(Complex::new(0.0, 0.0));
        let mut slot = (self.write + FFT_LEN - self.filled) % FFT_LEN;
        for pos in pad..FFT_LEN {
            self.spectrum[pos] = Complex::new(self.ring[slot] * self.window[pos], 0.0);
            slot += 1;
            if slot == FFT_LEN {
                slot = 0;
            }
        }
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);

        // 2. Magnitude of each bin, normalized so a full-scale sine reads ≈ 1.0.
        let scale = 4.0 / FFT_LEN as f32;
        let bw = sample_rate as f32 / FFT_LEN as f32; // bin width in Hz

        // 3–5. Per band: max magnitude over its bin range, dB-mapped, tilted.
        for (i, level) in out.iter_mut().enumerate() {
            let lo = (self.edges[i] / bw).floor() as usize;
            let hi = ((self.edges[i + 1] / bw).ceil() as usize).max(lo);
            let mut mag = 0.0f32;
            for bin in lo..=hi.min(BINS - 1) {
                mag = mag.max(self.spectrum[bin].norm() * scale);
            }
            let center = (self.edges[i] * self.edges[i + 1]).sqrt();
            // Tilt keeps highs visible; never *cuts* below 1 kHz.
            let tilt = (TILT_DB_PER_OCTAVE * (center / 1000.0).log2()).max(0.0);
            let db = 20.0 * mag.max(1e-6).log10() + tilt;
            *level = ((db + FLOOR_DB) / FLOOR_DB).clamp(0.0, 1.0);
        }
    }

    /// Band edges `edge[i] = 40 × (hi / 40)^(i / BANDS)` for `i in 0..=BANDS`,
    /// with `hi = min(16 kHz, 0.45 × rate)` — always below Nyquist.
    fn update_edges(&mut self, sample_rate: u32) {
        if self.edge_rate == sample_rate {
            return;
        }
        let hi = HI_CAP_HZ.min(0.45 * sample_rate as f32);
        let ratio = hi / LO_HZ;
        for (i, edge) in self.edges.iter_mut().enumerate() {
            *edge = LO_HZ * ratio.powf(i as f32 / BANDS as f32);
        }
        self.edge_rate = sample_rate;
    }
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-scale sine, `seconds` long, at `rate` (mono).
    fn sine(freq: f32, rate: u32, seconds: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|n| (2.0 * std::f32::consts::PI * freq * n as f32 / rate as f32).sin())
            .collect()
    }

    /// The band whose range contains `freq` — mirrors the `bands` edge math.
    fn band_of(freq: f32, rate: u32) -> usize {
        let mut a = Analyzer::new();
        a.update_edges(rate);
        (0..BANDS)
            .find(|&i| a.edges[i + 1] >= freq)
            .unwrap_or(BANDS - 1)
    }

    fn levels_of(sig: &[f32], rate: u32) -> [f32; BANDS] {
        let mut a = Analyzer::new();
        a.push(sig);
        let mut out = [0.0; BANDS];
        a.bands(rate, &mut out);
        out
    }

    fn argmax(out: &[f32; BANDS]) -> usize {
        out.iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .expect("BANDS > 0")
    }

    #[test]
    fn one_khz_sine_peaks_in_its_band_and_is_loud() {
        let rate = 44_100;
        let out = levels_of(&sine(1000.0, rate, 0.2), rate);
        let peak = band_of(1000.0, rate);
        assert_eq!(argmax(&out), peak, "1 kHz must peak in its own band");
        assert!(out[peak] > 0.8, "full-scale sine reads loud: {}", out[peak]);
        // Two bands away: the Hann leakage is far below −30 dB.
        if peak >= 2 {
            assert!(out[peak - 2] < 0.4, "left neighbour: {}", out[peak - 2]);
        }
        if peak + 2 < BANDS {
            assert!(out[peak + 2] < 0.4, "right neighbour: {}", out[peak + 2]);
        }
    }

    #[test]
    fn a_low_sine_peaks_below_a_high_sine() {
        let rate = 44_100;
        let low = argmax(&levels_of(&sine(100.0, rate, 0.2), rate));
        let high = argmax(&levels_of(&sine(1000.0, rate, 0.2), rate));
        assert!(
            low < high,
            "100 Hz band {low} must be below 1 kHz band {high}"
        );
    }

    #[test]
    fn silence_is_all_zero() {
        for rate in [44_100u32, 48_000, 96_000] {
            let out = levels_of(&vec![0.0; 4096], rate);
            assert!(
                out.iter().all(|v| *v == 0.0),
                "silence at {rate} Hz: {out:?}"
            );
        }
    }

    #[test]
    fn band_edges_stay_below_nyquist_at_every_rate() {
        for rate in [44_100u32, 48_000, 96_000] {
            let mut a = Analyzer::new();
            a.update_edges(rate);
            let hi = *a.edges.last().expect("BANDS + 1 edges");
            assert!(
                hi <= 0.45 * rate as f32 && hi < rate as f32 / 2.0,
                "top edge {hi} Hz at {rate} Hz"
            );
            assert!(a.edges[0] == LO_HZ, "bottom edge stays 40 Hz");
            // Same sine, three rates: every one produces a clear peak.
            let out = levels_of(&sine(1000.0, rate, 0.2), rate);
            assert!(
                out[argmax(&out)] > 0.8,
                "1 kHz at {rate} Hz reads loud: {:?}",
                out[argmax(&out)]
            );
        }
    }

    #[test]
    fn streaming_push_equals_one_big_push() {
        let rate = 44_100;
        let sig = sine(1000.0, rate, 0.2);
        let mut a = Analyzer::new();
        for chunk in sig.chunks(7) {
            a.push(chunk);
        }
        let mut streamed = [0.0; BANDS];
        a.bands(rate, &mut streamed);
        let direct = levels_of(&sig, rate);
        for (i, (s, d)) in streamed.iter().zip(direct.iter()).enumerate() {
            assert!((s - d).abs() < 1e-6, "band {i}: {s} vs {d}");
        }
    }

    #[test]
    fn pushing_more_than_the_fft_length_keeps_only_the_newest() {
        let rate = 44_100;
        // Old silence, then a sine that overflows the ring entirely.
        let mut a = Analyzer::new();
        a.push(&vec![0.0; FFT_LEN * 2]);
        a.push(&sine(1000.0, rate, 0.1));
        // Overflow the ring with silence again: the peak must vanish.
        a.push(&vec![0.0; FFT_LEN * 2]);
        let mut out = [0.0; BANDS];
        a.bands(rate, &mut out);
        assert!(
            out.iter().all(|v| *v == 0.0),
            "old samples evicted: {out:?}"
        );
    }
}
