//! The refusal of a cell: a reason and a short message.

use std::fmt;

/// Why a cell was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The text is not in the documented form.
    Syntax,
    /// A coordinate is outside its degrees range.
    OutOfRange,
    /// A coordinate has more than seven decimal places.
    TooManyDecimals,
    /// A range is not `[start,end)`.
    NotHalfOpen,
    /// A range is empty (`start` is not before `end`).
    Empty,
    /// A bound is of another kind than the field's.
    KindMismatch,
    /// A bound is missing or infinite.
    Unbounded,
}

impl Reason {
    /// A short stable word.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Syntax => "syntax",
            Reason::OutOfRange => "out_of_range",
            Reason::TooManyDecimals => "too_many_decimals",
            Reason::NotHalfOpen => "not_half_open",
            Reason::Empty => "empty",
            Reason::KindMismatch => "kind_mismatch",
            Reason::Unbounded => "unbounded",
        }
    }
}

/// A refused cell. The message repeats at most the first 40 characters of the cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextError {
    /// Why.
    pub reason: Reason,
    message: String,
}

/// The most characters of a cell a message repeats.
pub const EXCERPT_CHARS: usize = 40;

/// The first [`EXCERPT_CHARS`] characters of `cell`, with `...` when cut.
pub(crate) fn excerpt(cell: &str) -> String {
    let mut chars = cell.chars();
    let head: String = chars.by_ref().take(EXCERPT_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

impl TextError {
    pub(crate) fn new(reason: Reason, what: &str, cell: &str) -> Self {
        Self {
            reason,
            message: format!("{what} (`{}`)", excerpt(cell)),
        }
    }

    pub(crate) fn plain(reason: Reason, what: &str) -> Self {
        Self {
            reason,
            message: what.to_owned(),
        }
    }

    /// The plain-language message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.message)
    }
}

impl std::error::Error for TextError {}
