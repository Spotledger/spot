//! A tiny seeded generator (SplitMix64), so a profile's schedule is the same on every run.

/// A deterministic 64-bit generator. Not for secrets.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `0` when `n` is `0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// A value in `lo..=hi` (`lo` when the range is empty).
    pub fn between(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            lo
        } else {
            lo + self.below(hi - lo + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_sequence() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert!((0..50).all(|_| a.next_u64() == b.next_u64()));
        let mut c = Rng::new(8);
        assert_ne!(Rng::new(7).next_u64(), c.next_u64());
    }

    #[test]
    fn ranges_stay_inside_their_bounds() {
        let mut rng = Rng::new(1);
        for _ in 0..1000 {
            assert!(rng.below(10) < 10);
            let v = rng.between(5, 8);
            assert!((5..=8).contains(&v));
        }
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.between(9, 3), 9);
    }
}
