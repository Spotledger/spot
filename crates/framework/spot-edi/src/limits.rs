//! The bounds a parse runs under (WP-INT-06.T1, brief item 1).
//!
//! A document from the wire is untrusted. Every parse enforces a byte cap, a segment cap and a
//! per-segment element cap, so a hostile or corrupt file fails fast with a typed error instead
//! of exhausting memory. The defaults are generous for real EDI (a large interchange is a few
//! megabytes) and are all caller-overridable.

/// The limits a parse runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The maximum size of the input, in bytes.
    pub max_bytes: usize,
    /// The maximum number of segments in the interchange.
    pub max_segments: usize,
    /// The maximum number of elements in one segment.
    pub max_elements: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_segments: 100_000,
            max_elements: 10_000,
        }
    }
}

impl Limits {
    /// Tight limits for tests and for callers that want a small ceiling.
    pub fn small() -> Self {
        Self {
            max_bytes: 4096,
            max_segments: 256,
            max_elements: 64,
        }
    }
}
