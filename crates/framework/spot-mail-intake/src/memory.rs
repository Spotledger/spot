//! In-memory implementations of the ports, for tests and single-node setups. They pass the same
//! contract suites as the durable stores.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use spot_coordinator::FencingToken;
use spot_mail::{MailboxCursor, Secret};
use time::OffsetDateTime;

use crate::marker::OwnMailKey;
use crate::ports::{
    CredentialError, CredentialSource, CursorStore, IntakeAudit, SeenStore, StoreError,
};
use crate::report::AuditEvent;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The guarded data is plain values; a panic elsewhere cannot leave it half-written.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`CursorStore`] in memory. [`fail_next_stores`](Self::fail_next_stores) simulates an
/// outage of the write path so a test can crash a poll between the sink and the cursor.
#[derive(Debug, Default)]
pub struct MemoryCursorStore {
    cursors: Mutex<HashMap<String, (MailboxCursor, u64)>>,
    fail_stores: AtomicU32,
}

impl MemoryCursorStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The next `n` calls of `store` fail with [`StoreError::Unavailable`] and change nothing.
    pub fn fail_next_stores(&self, n: u32) {
        self.fail_stores.store(n, Ordering::SeqCst);
    }

    /// The highest fencing token stored for `account`.
    pub fn fence_of(&self, account: &str) -> Option<u64> {
        lock(&self.cursors).get(account).map(|(_, fence)| *fence)
    }
}

#[async_trait]
impl CursorStore for MemoryCursorStore {
    async fn load(&self, account: &str) -> Result<Option<MailboxCursor>, StoreError> {
        Ok(lock(&self.cursors).get(account).map(|(cursor, _)| *cursor))
    }

    async fn store(
        &self,
        account: &str,
        cursor: MailboxCursor,
        fence: FencingToken,
    ) -> Result<(), StoreError> {
        let injected = self
            .fail_stores
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if injected {
            return Err(StoreError::Unavailable);
        }
        let mut cursors = lock(&self.cursors);
        if let Some((_, stored)) = cursors.get(account)
            && fence.0 < *stored
        {
            return Err(StoreError::StaleFence);
        }
        cursors.insert(account.to_owned(), (cursor, fence.0));
        Ok(())
    }
}

/// A [`SeenStore`] in memory.
#[derive(Debug, Default)]
pub struct MemorySeenStore {
    seen: Mutex<HashMap<(String, String), OffsetDateTime>>,
}

impl MemorySeenStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many digests are recorded.
    pub fn len(&self) -> usize {
        lock(&self.seen).len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl SeenStore for MemorySeenStore {
    async fn contains(&self, account: &str, digest: &str) -> Result<bool, StoreError> {
        Ok(lock(&self.seen).contains_key(&(account.to_owned(), digest.to_owned())))
    }

    async fn first_time(
        &self,
        account: &str,
        digest: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let mut seen = lock(&self.seen);
        let key = (account.to_owned(), digest.to_owned());
        if seen.contains_key(&key) {
            return Ok(false);
        }
        seen.insert(key, now);
        Ok(true)
    }

    async fn purge_older_than(&self, cutoff: OffsetDateTime) -> Result<u64, StoreError> {
        let mut seen = lock(&self.seen);
        let before = seen.len();
        seen.retain(|_, at| *at >= cutoff);
        Ok((before - seen.len()) as u64)
    }
}

/// An [`IntakeAudit`] that keeps its entries for a test to read.
#[derive(Debug, Default)]
pub struct MemoryAudit {
    entries: Mutex<Vec<(String, AuditEvent)>>,
}

impl MemoryAudit {
    /// An empty trail.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every entry so far, oldest first, as (account, event).
    pub fn entries(&self) -> Vec<(String, AuditEvent)> {
        lock(&self.entries).clone()
    }
}

impl IntakeAudit for MemoryAudit {
    fn record(&self, account: &str, event: AuditEvent) {
        lock(&self.entries).push((account.to_owned(), event));
    }
}

/// A [`CredentialSource`] over a fixed map of reference to password.
#[derive(Debug, Default)]
pub struct StaticCredentials {
    secrets: BTreeMap<String, String>,
    own_key: Option<OwnMailKey>,
}

impl StaticCredentials {
    /// An empty source: every reference is not found.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the source answer `key` as the own-mail key (WP-PLT-11.T16).
    pub fn with_own_mail_key(mut self, key: OwnMailKey) -> Self {
        self.own_key = Some(key);
        self
    }

    /// Adds `password` under `credential_ref`.
    pub fn with(mut self, credential_ref: &str, password: &str) -> Self {
        self.secrets
            .insert(credential_ref.to_owned(), password.to_owned());
        self
    }
}

#[async_trait]
impl CredentialSource for StaticCredentials {
    async fn resolve(&self, credential_ref: &str) -> Result<Secret, CredentialError> {
        self.secrets
            .get(credential_ref)
            .map(|password| Secret::new(password.clone()))
            .ok_or(CredentialError::NotFound)
    }

    async fn own_mail_key(&self) -> Option<OwnMailKey> {
        self.own_key.clone()
    }
}
