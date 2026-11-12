//! The `StoreStateStore` port.

use std::collections::BTreeSet;

use spot_registry::{AppName, Version};

use crate::error::Result;
use crate::model::{
    AuditEntry, AuditRecord, Begin, BundleRevocation, Fingerprint, IssuedList, KeyHash, KeyInfo,
    KeyRecord, KeyRevocation, ListingRecord, ListingState, ReviewRecord, ReviewUpdate,
};

/// The store server's own durable state. Synchronous and `Send + Sync`; every instant is an
/// argument (no implementation reads a clock); every failure is a visible `store_state.*`
/// error, never a default value (a database that is down is not `Ok(None)`).
pub trait StoreStateStore: Send + Sync {
    // ---- publisher keys --------------------------------------------------------------------

    /// Stores the hash of a new key for `publisher_id` and returns its id. A publisher may hold
    /// several keys (rotation).
    ///
    /// # Errors
    /// `store_state.duplicate_key_hash`; `store_state.too_large` for an empty or long id/label.
    fn issue_publisher_key(
        &self,
        publisher_id: &str,
        key_hash: KeyHash,
        label: &str,
        now: i64,
    ) -> Result<u64>;

    /// The key with this hash, revoked or not. `None` only when no such key is stored.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn lookup_publisher_key(&self, key_hash: &KeyHash) -> Result<Option<KeyRecord>>;

    /// Marks the key revoked at `now`. Revoking a revoked key keeps the first time.
    ///
    /// # Errors
    /// `store_state.key_unknown`.
    fn revoke_publisher_key(&self, key_id: u64, now: i64) -> Result<()>;

    /// The keys of one publisher, oldest first, without their hashes.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn list_publisher_keys(&self, publisher_id: &str) -> Result<Vec<KeyInfo>>;

    // ---- reviews and the listing -----------------------------------------------------------

    /// Stores a review. Idempotent on `review_id`: an existing review is left untouched and
    /// `false` is returned.
    ///
    /// # Errors
    /// `store_state.too_large`.
    fn create_review(&self, record: &ReviewRecord) -> Result<bool>;

    /// One review.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn get_review(&self, review_id: &str) -> Result<Option<ReviewRecord>>;

    /// Applies the worker's result to a review.
    ///
    /// # Errors
    /// `store_state.review_unknown`; `store_state.too_large`.
    fn update_review(&self, review_id: &str, update: &ReviewUpdate) -> Result<()>;

    /// Moves `(app, version)` to `state`, checked in the store as one atomic step. A missing row
    /// may become `PendingReview` only; `PendingReview` becomes `Listed` or `Rejected`; `Listed`
    /// becomes `Withdrawn`. Everything else, including a lost race, is refused.
    ///
    /// # Errors
    /// `store_state.bad_transition`.
    fn set_listing(
        &self,
        app: &AppName,
        version: &Version,
        state: ListingState,
        by: &str,
        now: i64,
    ) -> Result<()>;

    /// The stored listing; `None` means the version is not listed (fail closed).
    ///
    /// # Errors
    /// `store_state.backend`.
    fn listing(&self, app: &AppName, version: &Version) -> Result<Option<ListingRecord>>;

    /// Which of `candidates` hold an explicit `Listed` row, in one call (at most
    /// [`crate::MAX_CANDIDATES`]).
    ///
    /// # Errors
    /// `store_state.too_large` over the candidate limit.
    fn listed_among(
        &self,
        candidates: &[(AppName, Version)],
    ) -> Result<BTreeSet<(AppName, Version)>>;

    /// The reviews of one version, oldest first.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn reviews_for(&self, app: &AppName, version: &Version) -> Result<Vec<ReviewRecord>>;

    // ---- idempotency -----------------------------------------------------------------------

    /// Claims `(publisher_id, key)` for a request with `fingerprint`, atomically.
    ///
    /// # Errors
    /// `store_state.too_large`.
    fn idempotency_begin(
        &self,
        publisher_id: &str,
        key: &str,
        fingerprint: &Fingerprint,
        now: i64,
    ) -> Result<Begin>;

    /// Records the review a claimed key led to. Repeating it with the same review is fine.
    ///
    /// # Errors
    /// `store_state.review_unknown` when the review does not exist;
    /// `store_state.idempotency_conflict` when the key is absent or finished with another review.
    fn idempotency_finish(&self, publisher_id: &str, key: &str, review_id: &str) -> Result<()>;

    /// Deletes the idempotency rows created before `older_than`; returns how many.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn idempotency_prune(&self, older_than: i64) -> Result<u64>;

    // ---- revocation state ------------------------------------------------------------------

    /// Records a revoked signing key; the first record wins. Returns whether it was new.
    ///
    /// # Errors
    /// `store_state.too_large`.
    fn revoke_key(&self, revocation: &KeyRevocation) -> Result<bool>;

    /// Records a revoked bundle; the first record wins. Returns whether it was new.
    ///
    /// # Errors
    /// `store_state.too_large`.
    fn revoke_bundle(&self, revocation: &BundleRevocation) -> Result<bool>;

    /// Every revoked key, oldest first.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn revoked_keys(&self) -> Result<Vec<KeyRevocation>>;

    /// Every revoked bundle, oldest first.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn revoked_bundles(&self) -> Result<Vec<BundleRevocation>>;

    /// The next revocation list generation: atomic, strictly increasing across processes,
    /// starting at 1.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn next_generation(&self, now: i64) -> Result<u64>;

    /// Keeps `list` as the last issued revocation list. A list whose generation is not above the
    /// stored one is refused.
    ///
    /// # Errors
    /// `store_state.bad_transition` for an old generation; `store_state.too_large`.
    fn record_issued_list(&self, list: &IssuedList) -> Result<()>;

    /// The last issued list, byte for byte.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn last_issued_list(&self) -> Result<Option<IssuedList>>;

    // ---- audit -----------------------------------------------------------------------------

    /// Appends an entry (append-only) and returns its position.
    ///
    /// # Errors
    /// `store_state.too_large`.
    fn append_audit(&self, entry: &AuditEntry) -> Result<u64>;

    /// Up to `limit` (at most [`crate::MAX_AUDIT_PAGE`]) entries with an id above `after_id`,
    /// oldest first.
    ///
    /// # Errors
    /// `store_state.backend`.
    fn read_audit(&self, after_id: u64, limit: u32) -> Result<Vec<AuditRecord>>;
}
