//! A small deterministic hash (FNV-1a, 64 bit) for plan identity and admission jitter.
//!
//! It is an identity check against resuming the wrong plan and a spreading function, never a
//! security boundary: the cursor holds no secret and no record content, and the data the node
//! receives is protected by the transport and the snapshot's own signature.

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// An incremental FNV-1a hasher. Each field is length-prefixed so `("ab", "c")` and `("a", "bc")`
/// differ.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fnv(u64);

impl Fnv {
    pub(crate) fn new() -> Self {
        Self(OFFSET)
    }

    fn byte(&mut self, b: u8) {
        self.0 ^= u64::from(b);
        self.0 = self.0.wrapping_mul(PRIME);
    }

    pub(crate) fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        for b in s.bytes() {
            self.byte(b);
        }
    }

    pub(crate) fn finish(self) -> u64 {
        self.0
    }
}

/// The hash of one string.
pub(crate) fn of_str(s: &str) -> u64 {
    let mut h = Fnv::new();
    h.str(s);
    h.finish()
}
