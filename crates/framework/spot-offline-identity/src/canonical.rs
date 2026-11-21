//! The canonical byte form every hash and signature covers.
//!
//! Integers are big-endian; a field is a `u32` byte length then the bytes; a list is a `u32`
//! count then its items. Each signed object starts with its own domain tag, so a signature
//! over one kind of object can never verify as another.

/// A byte writer for the canonical form.
pub(crate) struct Enc(Vec<u8>);

impl Enc {
    /// Starts with a domain tag.
    pub(crate) fn new(domain: &str) -> Self {
        let mut bytes = domain.as_bytes().to_vec();
        bytes.push(0);
        Self(bytes)
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }

    pub(crate) fn len(&mut self, n: usize) -> &mut Self {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        self.0.extend_from_slice(&n.to_be_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.len(v.len());
        self.0.extend_from_slice(v);
        self
    }

    pub(crate) fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.0
    }
}
