//! Typed errors with stable `capture_invariant_pg.*` codes.
//!
//! Every variant is catalogued in
//! `crates/framework/spot-errors/catalogue/capture_invariant_pg.toml`. A message is a fixed
//! sentence: it never carries SQL text, a row, an entity name, a record id or a value.

/// What can go wrong reading the feed and the journal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapturePgError {
    /// The page size is zero, or the window is zero.
    #[error("the capture invariant's options are invalid")]
    OptionsInvalid,
    /// The connection did not give a read-only snapshot.
    #[error("the capture invariant refused a connection that is not read-only")]
    NotReadOnly,
    /// A statement failed, or no connection could be had.
    #[error("the capture invariant could not read the change feed or the journal")]
    ReadFailed,
    /// A row does not decode: a negative version, an unknown operation, a payload without one.
    #[error("a change feed or journal row could not be decoded")]
    Corrupt,
    /// The watermark could not be read or written.
    #[error("the capture invariant's watermark could not be read or written")]
    WatermarkFailed,
    /// The checker refused a stream (positions that do not increase, a duplicate feed entry).
    #[error("the capture invariant's checker refused a stream")]
    CheckerRefused,
}

impl CapturePgError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OptionsInvalid => "capture_invariant_pg.options_invalid",
            Self::NotReadOnly => "capture_invariant_pg.not_read_only",
            Self::ReadFailed => "capture_invariant_pg.read_failed",
            Self::Corrupt => "capture_invariant_pg.corrupt",
            Self::WatermarkFailed => "capture_invariant_pg.watermark_failed",
            Self::CheckerRefused => "capture_invariant_pg.checker_refused",
        }
    }
}
