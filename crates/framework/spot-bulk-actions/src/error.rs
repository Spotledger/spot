//! Whole-request refusals (WP-DOC-09.T6): a request that is refused before any document is
//! touched. Per-document problems are never these: they are [`BulkStatus::Failed`] results.
//!
//! [`BulkStatus::Failed`]: crate::BulkStatus::Failed

use std::fmt;

/// Stable codes of the whole-request refusals (catalogue: `bulk_actions.toml`).
pub mod codes {
    /// The request names more targets than `max_targets`.
    pub const TOO_MANY_TARGETS: &str = "bulk_actions.too_many_targets";
    /// The request names no target at all.
    pub const EMPTY_BATCH: &str = "bulk_actions.empty_batch";
    /// The batch label is empty, longer than 64 characters or holds a control character.
    pub const BAD_LABEL: &str = "bulk_actions.bad_label";
    /// The concurrency is outside 1 to 16.
    pub const BAD_CONCURRENCY: &str = "bulk_actions.bad_concurrency";
    /// The action itself is not allowed (an empty patch, a malformed method name).
    pub const ACTION_NOT_ALLOWED: &str = "bulk_actions.action_not_allowed";
}

/// A refused request. Carries a stable code and a fixed sentence; never a document value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkError {
    code: &'static str,
    message: &'static str,
}

impl BulkError {
    pub(crate) fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    /// The stable code, one of [`codes`].
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The plain-language message.
    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for BulkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BulkError {}
