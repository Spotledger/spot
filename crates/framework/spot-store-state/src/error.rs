//! The refusals of the store state: a stable `store_state.*` code and a fixed sentence.
//!
//! A message never carries a key, a key hash, a token, a summary, SQL, a connection string or a
//! row value; the only variable text is a stable code of the layer below (a bridge refusal).

use std::fmt;

/// Stable code: the listing transition is not in the table, or lost a race.
pub const BAD_TRANSITION: &str = "store_state.bad_transition";
/// Stable code: no publisher key with that id.
pub const KEY_UNKNOWN: &str = "store_state.key_unknown";
/// Stable code: a key with that hash is already stored.
pub const DUPLICATE_KEY_HASH: &str = "store_state.duplicate_key_hash";
/// Stable code: an idempotency key is already bound to another request or review.
pub const IDEMPOTENCY_CONFLICT: &str = "store_state.idempotency_conflict";
/// Stable code: no review with that id.
pub const REVIEW_UNKNOWN: &str = "store_state.review_unknown";
/// Stable code: a field is empty, too long or too large; nothing was truncated.
pub const TOO_LARGE: &str = "store_state.too_large";
/// Stable code: a review summary is not one JSON document.
pub const BAD_SUMMARY: &str = "store_state.bad_summary";
/// Stable code: the backend failed (database, bridge, poisoned lock, unreadable stored row).
pub const BACKEND: &str = "store_state.backend";

/// A refusal with a stable code and a fixed sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreStateError {
    /// The stable dotted code.
    pub code: &'static str,
    /// A fixed sentence; never secret or row data.
    pub message: String,
}

/// The result of a store-state call.
pub type Result<T> = std::result::Result<T, StoreStateError>;

impl StoreStateError {
    /// An error with `code` and a sentence the caller keeps free of secrets.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `store_state.bad_transition`.
    #[must_use]
    pub fn bad_transition() -> Self {
        Self::new(
            BAD_TRANSITION,
            "the listing state does not allow that transition",
        )
    }

    /// `store_state.key_unknown`.
    #[must_use]
    pub fn key_unknown() -> Self {
        Self::new(KEY_UNKNOWN, "no publisher key with that id")
    }

    /// `store_state.duplicate_key_hash`.
    #[must_use]
    pub fn duplicate_key_hash() -> Self {
        Self::new(DUPLICATE_KEY_HASH, "a publisher key with that hash exists")
    }

    /// `store_state.idempotency_conflict`.
    #[must_use]
    pub fn idempotency_conflict() -> Self {
        Self::new(
            IDEMPOTENCY_CONFLICT,
            "the idempotency key is bound to another request or review",
        )
    }

    /// `store_state.review_unknown`.
    #[must_use]
    pub fn review_unknown() -> Self {
        Self::new(REVIEW_UNKNOWN, "no review with that id")
    }

    /// `store_state.too_large`, naming the field (a fixed, caller-chosen word).
    #[must_use]
    pub fn too_large(field: &'static str) -> Self {
        Self::new(
            TOO_LARGE,
            format!("the {field} is empty, too long or too large"),
        )
    }

    /// `store_state.bad_summary`.
    #[must_use]
    pub fn bad_summary() -> Self {
        Self::new(BAD_SUMMARY, "the review summary is not one JSON document")
    }

    /// `store_state.backend` with a fixed sentence.
    #[must_use]
    pub fn backend(reason: &'static str) -> Self {
        Self::new(BACKEND, reason)
    }
}

impl fmt::Display for StoreStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StoreStateError {}
