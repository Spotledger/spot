//! The records the store state holds, and the bounds both implementations enforce.
//!
//! Every bounded field is checked by `validate`, which the memory double and the PostgreSQL
//! implementation both call, so an oversized value is refused (`store_state.too_large`) and never
//! truncated. Types that hold a key hash or a report summary print neither in `Debug`.

use std::fmt;

use spot_registry::{AppName, Digest, Version};

use crate::error::{Result, StoreStateError};

/// Longest identifier text (publisher id, review id, idempotency key, actor, key id), in bytes.
pub const MAX_ID: usize = 128;
/// Longest key label, in bytes.
pub const MAX_LABEL: usize = 128;
/// Longest reason, in bytes.
pub const MAX_REASON: usize = 1024;
/// Longest verdict or error-code text, in bytes.
pub const MAX_CODE: usize = 128;
/// Largest review summary, in bytes (64 KiB).
pub const MAX_SUMMARY: usize = 64 * 1024;
/// Largest stored revocation list body, in bytes (1 MiB).
pub const MAX_LIST_BODY: usize = 1024 * 1024;
/// Most candidates one `listed_among` call takes.
pub const MAX_CANDIDATES: usize = 1000;
/// Most rows one `read_audit` call returns.
pub const MAX_AUDIT_PAGE: u32 = 1000;

/// A non-empty text of at most `max` bytes.
pub(crate) fn bounded(text: &str, max: usize, field: &'static str) -> Result<()> {
    if text.is_empty() || text.len() > max {
        return Err(StoreStateError::too_large(field));
    }
    Ok(())
}

/// A text of at most `max` bytes that may be empty (a reason).
pub(crate) fn bounded_or_empty(text: &str, max: usize, field: &'static str) -> Result<()> {
    if text.len() > max {
        return Err(StoreStateError::too_large(field));
    }
    Ok(())
}

/// Checks an identifier (publisher id, review id, idempotency key, actor, key id).
///
/// # Errors
/// `store_state.too_large` when empty or longer than [`MAX_ID`].
pub fn check_id(text: &str, field: &'static str) -> Result<()> {
    bounded(text, MAX_ID, field)
}

/// The SHA-256 of a publisher key. The key itself is never stored; `Debug` does not print the
/// bytes, so a log line or a panic message that holds one reveals nothing.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyHash([u8; 32]);

impl KeyHash {
    /// Wraps a SHA-256 digest.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest bytes, for the caller's constant-time comparison.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for KeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyHash(..)")
    }
}

/// A stored publisher key, found by its hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRecord {
    /// The publisher the key is bound to.
    pub publisher_id: String,
    /// The key's id (not secret).
    pub key_id: u64,
    /// The stored hash, which the caller compares to the presented hash in constant time.
    pub key_hash: KeyHash,
    /// When the key was revoked; `None` while it is live.
    pub revoked_at: Option<i64>,
}

/// One key as listed for its publisher: no hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// The key's id.
    pub key_id: u64,
    /// The operator's label.
    pub label: String,
    /// When it was issued.
    pub issued_at: i64,
    /// When it was revoked.
    pub revoked_at: Option<i64>,
}

/// Where a review is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewStatus {
    /// Waiting for the worker.
    Queued,
    /// The worker has it.
    Running,
    /// Finished with a verdict.
    Done,
    /// Finished without a verdict; an error code says why.
    Failed,
}

impl ReviewStatus {
    /// The stored word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// The status for a stored word.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A review summary: one JSON document of at most [`MAX_SUMMARY`] bytes, stored and returned
/// byte for byte. `Debug` shows its length only (it can quote the reviewed bundle).
#[derive(Clone, PartialEq, Eq)]
pub struct ReviewSummary(String);

impl ReviewSummary {
    /// Checks that `json` is within the bound and parses as one JSON document.
    ///
    /// # Errors
    /// `store_state.too_large` over 64 KiB (never truncated); `store_state.bad_summary` when it
    /// is not JSON.
    pub fn new(json: String) -> Result<Self> {
        if json.len() > MAX_SUMMARY {
            return Err(StoreStateError::too_large("review summary"));
        }
        serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|_| StoreStateError::bad_summary())?;
        Ok(Self(json))
    }

    /// The JSON text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ReviewSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReviewSummary({} bytes)", self.0.len())
    }
}

/// A review of one submitted bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRecord {
    /// The review's id.
    pub review_id: String,
    /// The submitting publisher.
    pub publisher_id: String,
    /// The app under review.
    pub app: AppName,
    /// The version under review.
    pub version: Version,
    /// The bundle's canonical digest.
    pub digest: Digest,
    /// Where it is.
    pub status: ReviewStatus,
    /// The verdict word `spot-app-review` gave, once done.
    pub verdict: Option<String>,
    /// The bounded summary.
    pub summary: Option<ReviewSummary>,
    /// The error code of a failed review.
    pub error_code: Option<String>,
    /// When it was created.
    pub created_at: i64,
    /// When it last changed.
    pub updated_at: i64,
}

impl ReviewRecord {
    /// Checks the bounded fields.
    ///
    /// # Errors
    /// `store_state.too_large`.
    pub fn validate(&self) -> Result<()> {
        check_id(&self.review_id, "review id")?;
        check_id(&self.publisher_id, "publisher id")?;
        validate_outcome(self.verdict.as_deref(), self.error_code.as_deref())
    }
}

/// What the review worker writes back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewUpdate {
    /// The new status.
    pub status: ReviewStatus,
    /// The verdict word.
    pub verdict: Option<String>,
    /// The summary.
    pub summary: Option<ReviewSummary>,
    /// The error code of a failure.
    pub error_code: Option<String>,
    /// When.
    pub now: i64,
}

impl ReviewUpdate {
    /// Checks the bounded fields.
    ///
    /// # Errors
    /// `store_state.too_large`.
    pub fn validate(&self) -> Result<()> {
        validate_outcome(self.verdict.as_deref(), self.error_code.as_deref())
    }
}

fn validate_outcome(verdict: Option<&str>, error_code: Option<&str>) -> Result<()> {
    for (text, field) in [(verdict, "verdict"), (error_code, "error code")] {
        if let Some(text) = text {
            bounded(text, MAX_CODE, field)?;
        }
    }
    Ok(())
}

/// Whether a version is publicly listed. Only an explicit `Listed` row lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingState {
    /// Submitted, awaiting a decision. Not listed.
    PendingReview,
    /// Approved and listed.
    Listed,
    /// Refused. Not listed.
    Rejected,
    /// Taken down after listing. Not listed.
    Withdrawn,
}

impl ListingState {
    /// The stored word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingReview => "pending_review",
            Self::Listed => "listed",
            Self::Rejected => "rejected",
            Self::Withdrawn => "withdrawn",
        }
    }

    /// The state for a stored word.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "pending_review" => Some(Self::PendingReview),
            "listed" => Some(Self::Listed),
            "rejected" => Some(Self::Rejected),
            "withdrawn" => Some(Self::Withdrawn),
            _ => None,
        }
    }

    /// The states a row may be in for `self` to be the next one. Absence of a row is the
    /// start, and only `PendingReview` may start a row (see [`ListingState::may_start`]).
    #[must_use]
    pub fn allowed_previous(self) -> &'static [ListingState] {
        match self {
            Self::PendingReview => &[],
            Self::Listed | Self::Rejected => &[Self::PendingReview],
            Self::Withdrawn => &[Self::Listed],
        }
    }

    /// Whether this state may create the row.
    #[must_use]
    pub fn may_start(self) -> bool {
        self == Self::PendingReview
    }
}

/// The stored listing of one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingRecord {
    /// The state.
    pub state: ListingState,
    /// Who set it.
    pub by: String,
    /// When.
    pub at: i64,
}

/// The digest of an idempotent request (app, version and bundle digest), made by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint(pub [u8; 32]);

/// The answer of `idempotency_begin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Begin {
    /// First sight of the key: the caller proceeds and later calls `idempotency_finish`.
    New,
    /// The same key and request was seen: the stored result. `review_id` is `None` while the
    /// first request has not finished yet.
    Replay {
        /// The original review id.
        review_id: Option<String>,
    },
    /// The key was used with a different request.
    Conflict,
}

/// A revoked signing key (the text of its key id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRevocation {
    /// The key id text.
    pub key_id: String,
    /// Why.
    pub reason: String,
    /// When.
    pub at: i64,
}

impl KeyRevocation {
    /// Checks the bounded fields.
    ///
    /// # Errors
    /// `store_state.too_large`.
    pub fn validate(&self) -> Result<()> {
        check_id(&self.key_id, "key id")?;
        bounded_or_empty(&self.reason, MAX_REASON, "reason")
    }
}

/// A revoked bundle digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleRevocation {
    /// The bundle's canonical digest.
    pub digest: Digest,
    /// Why.
    pub reason: String,
    /// When.
    pub at: i64,
}

impl BundleRevocation {
    /// Checks the bounded fields.
    ///
    /// # Errors
    /// `store_state.too_large`.
    pub fn validate(&self) -> Result<()> {
        bounded_or_empty(&self.reason, MAX_REASON, "reason")
    }
}

/// The last signed revocation list the server issued, kept so it can be served after a restart
/// without re-signing and without the ETag changing. The body is stored byte for byte.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedList {
    /// Its generation.
    pub generation: u64,
    /// When it was issued.
    pub issued_at: i64,
    /// When clients should look again.
    pub next_update: i64,
    /// The signed document, as served.
    pub body_json: String,
}

impl IssuedList {
    /// Checks the bounds.
    ///
    /// # Errors
    /// `store_state.too_large`.
    pub fn validate(&self) -> Result<()> {
        if i64::try_from(self.generation).is_err() {
            return Err(StoreStateError::too_large("generation"));
        }
        bounded(&self.body_json, MAX_LIST_BODY, "list body")
    }
}

impl fmt::Debug for IssuedList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedList")
            .field("generation", &self.generation)
            .field("issued_at", &self.issued_at)
            .field("next_update", &self.next_update)
            .field("body_bytes", &self.body_json.len())
            .finish()
    }
}

/// What an audit entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditKind {
    /// A bundle was published.
    Publish,
    /// A release was yanked.
    Yank,
    /// A release was withdrawn.
    Withdraw,
    /// A publisher was revoked.
    RevokePublisher,
    /// A signing key was revoked.
    RevokeKey,
    /// A bundle was revoked.
    RevokeBundle,
    /// A review was approved.
    Approve,
    /// A review was rejected.
    Reject,
    /// A publisher key was issued.
    KeyIssue,
    /// A publisher key was revoked.
    KeyRevoke,
}

impl AuditKind {
    /// The stored word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Yank => "yank",
            Self::Withdraw => "withdraw",
            Self::RevokePublisher => "revoke_publisher",
            Self::RevokeKey => "revoke_key",
            Self::RevokeBundle => "revoke_bundle",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::KeyIssue => "key_issue",
            Self::KeyRevoke => "key_revoke",
        }
    }

    /// The kind for a stored word.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        [
            Self::Publish,
            Self::Yank,
            Self::Withdraw,
            Self::RevokePublisher,
            Self::RevokeKey,
            Self::RevokeBundle,
            Self::Approve,
            Self::Reject,
            Self::KeyIssue,
            Self::KeyRevoke,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == word)
    }
}

/// One audit entry: who did what to which subject, never a key, token, bundle or request body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// What happened.
    pub kind: AuditKind,
    /// Who did it.
    pub actor: String,
    /// The publisher it concerns.
    pub publisher_id: Option<String>,
    /// The app it concerns.
    pub app: Option<AppName>,
    /// The version it concerns.
    pub version: Option<Version>,
    /// The bundle digest it concerns.
    pub digest: Option<Digest>,
    /// A bounded reason.
    pub reason: String,
    /// When.
    pub at: i64,
}

impl AuditEntry {
    /// Checks the bounded fields.
    ///
    /// # Errors
    /// `store_state.too_large`.
    pub fn validate(&self) -> Result<()> {
        check_id(&self.actor, "actor")?;
        if let Some(publisher) = &self.publisher_id {
            check_id(publisher, "publisher id")?;
        }
        bounded_or_empty(&self.reason, MAX_REASON, "reason")
    }
}

/// An audit entry as stored: with its position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// Strictly increasing position.
    pub id: u64,
    /// The entry.
    pub entry: AuditEntry,
}
