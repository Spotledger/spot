//! [`MemoryStoreState`]: the reference implementation and the test double. It obeys the same
//! rules as the PostgreSQL implementation, which is held to the same shared cases.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use spot_registry::{AppName, Version};

use crate::error::{Result, StoreStateError};
use crate::model::{
    AuditEntry, AuditRecord, Begin, BundleRevocation, Fingerprint, IssuedList, KeyHash, KeyInfo,
    KeyRecord, KeyRevocation, ListingRecord, ListingState, MAX_AUDIT_PAGE, MAX_CANDIDATES,
    MAX_LABEL, ReviewRecord, ReviewUpdate, bounded, check_id,
};
use crate::store::StoreStateStore;

struct KeyRow {
    publisher_id: String,
    hash: KeyHash,
    label: String,
    issued_at: i64,
    revoked_at: Option<i64>,
}

struct IdempotencyRow {
    fingerprint: Fingerprint,
    review_id: Option<String>,
    created_at: i64,
}

#[derive(Default)]
struct State {
    keys: BTreeMap<u64, KeyRow>,
    next_key: u64,
    reviews: BTreeMap<String, ReviewRecord>,
    listings: BTreeMap<(AppName, Version), ListingRecord>,
    idempotency: BTreeMap<(String, String), IdempotencyRow>,
    revoked_keys: BTreeMap<String, KeyRevocation>,
    revoked_bundles: BTreeMap<String, BundleRevocation>,
    generation: u64,
    issued: Option<IssuedList>,
    audit: Vec<AuditRecord>,
}

/// An in-memory [`StoreStateStore`]. One mutex guards the whole state, so every method is atomic.
#[derive(Default)]
pub struct MemoryStoreState {
    state: Mutex<State>,
}

impl std::fmt::Debug for MemoryStoreState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStoreState").finish_non_exhaustive()
    }
}

impl MemoryStoreState {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| StoreStateError::backend("the in-memory store state lock is poisoned"))
    }
}

impl StoreStateStore for MemoryStoreState {
    fn issue_publisher_key(
        &self,
        publisher_id: &str,
        key_hash: KeyHash,
        label: &str,
        now: i64,
    ) -> Result<u64> {
        check_id(publisher_id, "publisher id")?;
        bounded(label, MAX_LABEL, "label")?;
        let mut state = self.lock()?;
        if state.keys.values().any(|row| row.hash == key_hash) {
            return Err(StoreStateError::duplicate_key_hash());
        }
        state.next_key += 1;
        let id = state.next_key;
        state.keys.insert(
            id,
            KeyRow {
                publisher_id: publisher_id.to_owned(),
                hash: key_hash,
                label: label.to_owned(),
                issued_at: now,
                revoked_at: None,
            },
        );
        Ok(id)
    }

    fn lookup_publisher_key(&self, key_hash: &KeyHash) -> Result<Option<KeyRecord>> {
        let state = self.lock()?;
        Ok(state
            .keys
            .iter()
            .find(|(_, row)| row.hash == *key_hash)
            .map(|(id, row)| KeyRecord {
                publisher_id: row.publisher_id.clone(),
                key_id: *id,
                key_hash: row.hash,
                revoked_at: row.revoked_at,
            }))
    }

    fn revoke_publisher_key(&self, key_id: u64, now: i64) -> Result<()> {
        let mut state = self.lock()?;
        let row = state
            .keys
            .get_mut(&key_id)
            .ok_or_else(StoreStateError::key_unknown)?;
        row.revoked_at.get_or_insert(now);
        Ok(())
    }

    fn list_publisher_keys(&self, publisher_id: &str) -> Result<Vec<KeyInfo>> {
        let state = self.lock()?;
        Ok(state
            .keys
            .iter()
            .filter(|(_, row)| row.publisher_id == publisher_id)
            .map(|(id, row)| KeyInfo {
                key_id: *id,
                label: row.label.clone(),
                issued_at: row.issued_at,
                revoked_at: row.revoked_at,
            })
            .collect())
    }

    fn create_review(&self, record: &ReviewRecord) -> Result<bool> {
        record.validate()?;
        let mut state = self.lock()?;
        if state.reviews.contains_key(&record.review_id) {
            return Ok(false);
        }
        state
            .reviews
            .insert(record.review_id.clone(), record.clone());
        Ok(true)
    }

    fn get_review(&self, review_id: &str) -> Result<Option<ReviewRecord>> {
        Ok(self.lock()?.reviews.get(review_id).cloned())
    }

    fn update_review(&self, review_id: &str, update: &ReviewUpdate) -> Result<()> {
        update.validate()?;
        let mut state = self.lock()?;
        let review = state
            .reviews
            .get_mut(review_id)
            .ok_or_else(StoreStateError::review_unknown)?;
        review.status = update.status;
        review.verdict.clone_from(&update.verdict);
        review.summary.clone_from(&update.summary);
        review.error_code.clone_from(&update.error_code);
        review.updated_at = update.now;
        Ok(())
    }

    fn set_listing(
        &self,
        app: &AppName,
        version: &Version,
        state: ListingState,
        by: &str,
        now: i64,
    ) -> Result<()> {
        check_id(by, "actor")?;
        let mut guard = self.lock()?;
        let key = (app.clone(), version.clone());
        match guard.listings.get_mut(&key) {
            None if state.may_start() => {
                guard.listings.insert(
                    key,
                    ListingRecord {
                        state,
                        by: by.to_owned(),
                        at: now,
                    },
                );
                Ok(())
            }
            Some(row) if state.allowed_previous().contains(&row.state) => {
                *row = ListingRecord {
                    state,
                    by: by.to_owned(),
                    at: now,
                };
                Ok(())
            }
            _ => Err(StoreStateError::bad_transition()),
        }
    }

    fn listing(&self, app: &AppName, version: &Version) -> Result<Option<ListingRecord>> {
        Ok(self
            .lock()?
            .listings
            .get(&(app.clone(), version.clone()))
            .cloned())
    }

    fn listed_among(
        &self,
        candidates: &[(AppName, Version)],
    ) -> Result<BTreeSet<(AppName, Version)>> {
        if candidates.len() > MAX_CANDIDATES {
            return Err(StoreStateError::too_large("candidate list"));
        }
        let state = self.lock()?;
        Ok(candidates
            .iter()
            .filter(|key| {
                state
                    .listings
                    .get(*key)
                    .is_some_and(|row| row.state == ListingState::Listed)
            })
            .cloned()
            .collect())
    }

    fn reviews_for(&self, app: &AppName, version: &Version) -> Result<Vec<ReviewRecord>> {
        let state = self.lock()?;
        let mut rows: Vec<ReviewRecord> = state
            .reviews
            .values()
            .filter(|r| r.app == *app && r.version == *version)
            .cloned()
            .collect();
        rows.sort_by(|a, b| (a.created_at, &a.review_id).cmp(&(b.created_at, &b.review_id)));
        Ok(rows)
    }

    fn idempotency_begin(
        &self,
        publisher_id: &str,
        key: &str,
        fingerprint: &Fingerprint,
        now: i64,
    ) -> Result<Begin> {
        check_id(publisher_id, "publisher id")?;
        check_id(key, "idempotency key")?;
        let mut state = self.lock()?;
        let slot = (publisher_id.to_owned(), key.to_owned());
        if let Some(row) = state.idempotency.get(&slot) {
            return Ok(if row.fingerprint == *fingerprint {
                Begin::Replay {
                    review_id: row.review_id.clone(),
                }
            } else {
                Begin::Conflict
            });
        }
        state.idempotency.insert(
            slot,
            IdempotencyRow {
                fingerprint: *fingerprint,
                review_id: None,
                created_at: now,
            },
        );
        Ok(Begin::New)
    }

    fn idempotency_finish(&self, publisher_id: &str, key: &str, review_id: &str) -> Result<()> {
        let mut state = self.lock()?;
        if !state.reviews.contains_key(review_id) {
            return Err(StoreStateError::review_unknown());
        }
        let row = state
            .idempotency
            .get_mut(&(publisher_id.to_owned(), key.to_owned()))
            .ok_or_else(StoreStateError::idempotency_conflict)?;
        match &row.review_id {
            Some(existing) if existing != review_id => Err(StoreStateError::idempotency_conflict()),
            _ => {
                row.review_id = Some(review_id.to_owned());
                Ok(())
            }
        }
    }

    fn idempotency_prune(&self, older_than: i64) -> Result<u64> {
        let mut state = self.lock()?;
        let before = state.idempotency.len();
        state
            .idempotency
            .retain(|_, row| row.created_at >= older_than);
        Ok(u64::try_from(before - state.idempotency.len()).unwrap_or(u64::MAX))
    }

    fn revoke_key(&self, revocation: &KeyRevocation) -> Result<bool> {
        revocation.validate()?;
        let mut state = self.lock()?;
        if state.revoked_keys.contains_key(&revocation.key_id) {
            return Ok(false);
        }
        state
            .revoked_keys
            .insert(revocation.key_id.clone(), revocation.clone());
        Ok(true)
    }

    fn revoke_bundle(&self, revocation: &BundleRevocation) -> Result<bool> {
        revocation.validate()?;
        let mut state = self.lock()?;
        let digest = revocation.digest.to_hex();
        if state.revoked_bundles.contains_key(&digest) {
            return Ok(false);
        }
        state.revoked_bundles.insert(digest, revocation.clone());
        Ok(true)
    }

    fn revoked_keys(&self) -> Result<Vec<KeyRevocation>> {
        let mut rows: Vec<_> = self.lock()?.revoked_keys.values().cloned().collect();
        rows.sort_by(|a, b| (a.at, &a.key_id).cmp(&(b.at, &b.key_id)));
        Ok(rows)
    }

    fn revoked_bundles(&self) -> Result<Vec<BundleRevocation>> {
        let mut rows: Vec<_> = self.lock()?.revoked_bundles.values().cloned().collect();
        rows.sort_by_key(|r| (r.at, r.digest.to_hex()));
        Ok(rows)
    }

    fn next_generation(&self, _now: i64) -> Result<u64> {
        let mut state = self.lock()?;
        state.generation += 1;
        Ok(state.generation)
    }

    fn record_issued_list(&self, list: &IssuedList) -> Result<()> {
        list.validate()?;
        let mut state = self.lock()?;
        if state
            .issued
            .as_ref()
            .is_some_and(|stored| stored.generation >= list.generation)
        {
            return Err(StoreStateError::bad_transition());
        }
        state.issued = Some(list.clone());
        Ok(())
    }

    fn last_issued_list(&self) -> Result<Option<IssuedList>> {
        Ok(self.lock()?.issued.clone())
    }

    fn append_audit(&self, entry: &AuditEntry) -> Result<u64> {
        entry.validate()?;
        let mut state = self.lock()?;
        let id = u64::try_from(state.audit.len()).unwrap_or(u64::MAX) + 1;
        state.audit.push(AuditRecord {
            id,
            entry: entry.clone(),
        });
        Ok(id)
    }

    fn read_audit(&self, after_id: u64, limit: u32) -> Result<Vec<AuditRecord>> {
        let limit = usize::try_from(limit.min(MAX_AUDIT_PAGE)).unwrap_or(0);
        Ok(self
            .lock()?
            .audit
            .iter()
            .filter(|record| record.id > after_id)
            .take(limit)
            .cloned()
            .collect())
    }
}
