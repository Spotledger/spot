//! Shared SHA-256 helpers: one domain-separated canonical hasher and lowercase hex.

use sha2::{Digest, Sha256};

/// Lowercases the bytes as hex, without a hex crate.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// A canonical, domain-separated hasher.
///
/// The domain prefix binds a digest to its purpose; fields are separated by a NUL byte and
/// records by a newline, so `("ab", "c")` and `("a", "bc")` never collide.
pub(crate) struct CanonicalHasher {
    hasher: Sha256,
}

impl CanonicalHasher {
    /// Starts a hash for `domain`.
    pub(crate) fn new(domain: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update(b"\n");
        Self { hasher }
    }

    /// Adds a field's bytes.
    pub(crate) fn field(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.hasher.update(b"\0");
    }

    /// Adds a text field.
    pub(crate) fn text(&mut self, text: &str) {
        self.field(text.as_bytes());
    }

    /// Ends a record.
    pub(crate) fn end_record(&mut self) {
        self.hasher.update(b"\n");
    }

    /// Finishes as `sha256:<hex>`.
    pub(crate) fn finish(self) -> String {
        format!("sha256:{}", hex(&self.hasher.finalize()))
    }
}
