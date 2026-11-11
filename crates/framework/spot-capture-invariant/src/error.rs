//! The one error type of this crate, with stable `capture_invariant.*` codes.
//!
//! Messages name the stream or the field, never a document id or a value.

/// A stream was fed wrongly or a configuration is unusable. Findings about the data itself are
/// not errors: they are [`Finding`](crate::Finding)s in the [`Report`](crate::Report).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureInvariantError {
    /// A stream delivered a position that is not greater than the one before it.
    #[error("the {0} stream is not in strictly increasing position order")]
    StreamOutOfOrder(&'static str),

    /// A second feed entry for a document version already tracked.
    #[error("the change feed holds two entries for the same document and version")]
    FeedDuplicate,

    /// The window must hold at least one entry.
    #[error("the window must be at least 1")]
    WindowInvalid,

    /// An exclusion with an empty entity, owner or reason, or two for one entity.
    #[error("invalid exclusion: {0}")]
    ExclusionInvalid(&'static str),

    /// The report could not be encoded as JSON.
    #[error("the report could not be encoded")]
    ReportEncoding,
}

/// A position is not greater than the previous one on the same stream.
pub const STREAM_OUT_OF_ORDER: &str = "capture_invariant.stream_out_of_order";
/// Two change-feed entries for the same document version.
pub const FEED_DUPLICATE: &str = "capture_invariant.feed_duplicate";
/// The window size is zero.
pub const WINDOW_INVALID: &str = "capture_invariant.window_invalid";
/// An exclusion is incomplete or repeated.
pub const EXCLUSION_INVALID: &str = "capture_invariant.exclusion_invalid";
/// The report could not be encoded.
pub const REPORT_ENCODING: &str = "capture_invariant.report_encoding";

impl CaptureInvariantError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StreamOutOfOrder(_) => STREAM_OUT_OF_ORDER,
            Self::FeedDuplicate => FEED_DUPLICATE,
            Self::WindowInvalid => WINDOW_INVALID,
            Self::ExclusionInvalid(_) => EXCLUSION_INVALID,
            Self::ReportEncoding => REPORT_ENCODING,
        }
    }
}
