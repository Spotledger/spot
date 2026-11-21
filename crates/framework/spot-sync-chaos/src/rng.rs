//! One seeded generator, so a failing world replays exactly.

/// A seeded xorshift64* generator.
///
/// The World and its wire use this and nothing else: no thread RNG, no operating-system entropy,
/// no clock. Two generators built from one seed produce the same stream, which is what makes a
/// failing seed replayable.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed. A zero seed is mapped to a non-zero state.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value below `n`; `0` when `n` is `0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True with probability `per_mille` in a thousand.
    pub fn chance(&mut self, per_mille: u64) -> bool {
        per_mille > 0 && self.below(1_000) < per_mille.min(1_000)
    }

    /// Shuffle in place, Fisher-Yates, drawing only from this generator.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        let len = items.len();
        if len < 2 {
            return;
        }
        for i in (1..len).rev() {
            let j = usize::try_from(self.below(u64::try_from(i).unwrap_or(u64::MAX) + 1))
                .unwrap_or(0)
                .min(i);
            items.swap(i, j);
        }
    }

    /// One value of `choices`, or `None` for an empty slice.
    pub fn pick<'a, T>(&mut self, choices: &'a [T]) -> Option<&'a T> {
        if choices.is_empty() {
            return None;
        }
        let index = usize::try_from(self.below(u64::try_from(choices.len()).unwrap_or(1)))
            .unwrap_or(0)
            .min(choices.len() - 1);
        choices.get(index)
    }
}
