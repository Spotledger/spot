//! Injected randomness for pairing nonces. Randomness is a seam like the clock: the
//! trait is here, the deterministic source is here for tests, and the production source
//! is supplied by the caller (the service host), which owns the OS-entropy dependency.
//! No unit test depends on the machine's entropy and none sleeps.

use std::sync::Mutex;

/// A source of fresh bytes for one-time nonces. Nonces come from here, never from an
/// ad-hoc generator, so tests inject a fixed stream and pairing is deterministic.
///
/// A production implementation must draw from the operating system's CSPRNG (as
/// `spot-agent-enroll` and `spot-auth` do through `rand`'s `os_rng`) and refuse to
/// produce material when the OS source fails: an unpredictable nonce is what makes a
/// recorded code useless to an attacker who replayed network traffic.
pub trait RandomSource: Send + Sync + 'static {
    /// Fills `dest` with fresh random bytes. Implementations panic or refuse rather
    /// than hand out predictable material.
    fn fill(&self, dest: &mut [u8]);
}

/// A deterministic source for tests and examples: hands out a fixed byte sequence,
/// restarting from the first byte when exhausted. Every pairing test pins its expected
/// code and MAC to a stream given here.
#[derive(Debug)]
pub struct FixedRandom {
    bytes: Mutex<Vec<u8>>,
    cursor: Mutex<usize>,
}

impl FixedRandom {
    /// A stream that repeats `bytes` forever.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Mutex::new(bytes),
            cursor: Mutex::new(0),
        }
    }
}

impl RandomSource for FixedRandom {
    fn fill(&self, dest: &mut [u8]) {
        let bytes = self
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut cursor = self
            .cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in dest.iter_mut() {
            if *cursor >= bytes.len() {
                *cursor = 0;
            }
            *slot = bytes[*cursor];
            *cursor += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FixedRandom, RandomSource};

    #[test]
    fn fixed_random_repeats_when_exhausted() {
        let src = FixedRandom::new(vec![1, 2, 3]);
        let mut out = [0u8; 5];
        src.fill(&mut out);
        assert_eq!(out, [1, 2, 3, 1, 2]);
    }
}
