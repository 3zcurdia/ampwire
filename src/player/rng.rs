//! The player's tiny xorshift PRNG, shared by the mock's spectrum wobble and
//! the real player's shuffle (plan 03, step 5). Seeded → deterministic, so
//! shuffle tests are reproducible and the golden frames never change.

/// Tiny xorshift64 PRNG — enough for spectrum wobble and Fisher–Yates, no
/// dependency.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A seed; `0` is forced to `1` because xorshift never leaves an all-zero
    /// state.
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// One xorshift step, returning the new state.
    fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A float in `0.0..=1.0` (24 bits of mantissa).
    pub fn next_f32(&mut self) -> f32 {
        (self.step() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// An index in `0..n`. `n <= 1` always yields `0`; the modulo bias is
    /// irrelevant for shuffling a playlist.
    pub fn next_below(&mut self, n: usize) -> usize {
        if n <= 1 {
            0
        } else {
            (self.step() % n as u64) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_zero_still_produces_random_numbers() {
        let mut rng = Rng::new(0);
        assert_ne!(rng.next_f32(), rng.next_f32());
    }

    #[test]
    fn next_below_stays_in_range() {
        let mut rng = Rng::new(7);
        for _ in 0..200 {
            assert!(rng.next_below(5) < 5);
        }
        assert_eq!(rng.next_below(1), 0);
        assert_eq!(rng.next_below(0), 0);
    }

    #[test]
    fn same_seed_same_sequence() {
        let a: Vec<f32> = {
            let mut rng = Rng::new(42);
            (0..8).map(|_| rng.next_f32()).collect()
        };
        let b: Vec<f32> = {
            let mut rng = Rng::new(42);
            (0..8).map(|_| rng.next_f32()).collect()
        };
        assert_eq!(a, b);
    }
}
