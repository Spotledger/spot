//! The errors of the tail chunk codec: the A.3 rule that failed, never an entry line or a key.

use std::fmt;

/// Which validity rule of ADR-061 Appendix A.3 a chunk failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// (a) the key is a canonical tail key of the generation being read.
    A,
    /// (b) the object is at least 32 bytes and its last 32 equal the checksum (v1) or the MAC (v2).
    B,
    /// (c) the body starts with a known magic and `count` consumes it exactly.
    C,
    /// (d) every entry carries the key's generation and `count >= 1`.
    D,
    /// (e) inside the chunk `seq` is strictly consecutive.
    E,
}

impl Rule {
    /// The letter of the rule in A.3.
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::A => 'a',
            Self::B => 'b',
            Self::C => 'c',
            Self::D => 'd',
            Self::E => 'e',
        }
    }
}

/// A chunk that does not satisfy A.3 (a) to (e). The text names the rule and a fixed reason; it
/// never carries a line, a key id or a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkError {
    rule: Rule,
    reason: &'static str,
}

impl ChunkError {
    pub(crate) fn new(rule: Rule, reason: &'static str) -> Self {
        Self { rule, reason }
    }

    /// The A.3 rule that failed.
    #[must_use]
    pub fn rule(&self) -> Rule {
        self.rule
    }

    /// A fixed sentence saying what failed.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// The sentence `resume` reports as `journal_relay.chain_broken`: one fixed text per rule.
    #[must_use]
    pub(crate) fn chain_reason(&self) -> &'static str {
        match self.rule {
            Rule::A => "a tail chunk fails A.3 (a): its key is not a canonical tail key here",
            Rule::B => "a tail chunk fails A.3 (b): its checksum or MAC does not verify",
            Rule::C => "a tail chunk fails A.3 (c): its body is not exactly one chunk",
            Rule::D => "a tail chunk fails A.3 (d): wrong generation or no entry",
            Rule::E => "a tail chunk fails A.3 (e): its sequence numbers are not consecutive",
        }
    }
}

impl fmt::Display for ChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the tail chunk fails A.3 ({}): {}",
            self.rule.letter(),
            self.reason
        )
    }
}

impl std::error::Error for ChunkError {}

/// Why a batch could not be encoded as a version 2 chunk. Nothing was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The chunk would be larger than [`crate::tail::MAX_CHUNK_BYTES`] (A.7).
    TooLarge {
        /// The size the chunk reached when the encoder stopped (at least the limit).
        bytes: u64,
    },
    /// More entries than a `u32` counts, or a line longer than a `u32`.
    Count,
    /// A line is not UTF-8 (the AEAD seals text).
    NotUtf8,
    /// The key provider failed to wrap a data key, derive the MAC key or seal a line.
    Key,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge { .. } => "the batch does not fit one tail chunk",
            Self::Count => "the batch has too many entries or a line is too long",
            Self::NotUtf8 => "a line is not valid UTF-8",
            Self::Key => "the tail key provider failed",
        })
    }
}

impl std::error::Error for EncodeError {}
