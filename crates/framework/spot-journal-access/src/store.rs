//! The grant store trait and an in-memory store.
//!
//! Every mutating method takes the audit event it belongs to: a store writes the grant row and
//! the event **together** (the PostgreSQL store in one transaction), so no grant exists without
//! its record and no record without its grant.

use std::sync::Mutex;

use crate::error::AccessError;
use crate::model::{Fut, Grant, GrantEvent};

/// Where `journal_reader` grants live.
pub trait GrantStore: Send + Sync {
    /// Stores a new grant and its `granted` event.
    fn insert<'a>(&'a self, grant: &'a Grant, event: &'a GrantEvent) -> Fut<'a, ()>;

    /// Sets a live (not revoked) grant's expiry and writes the `renewed` event. Returns the
    /// updated grant; [`AccessError::GrantUnknown`] when there is no such live grant.
    fn renew<'a>(
        &'a self,
        id: &'a str,
        expires_at_millis: i64,
        event: &'a GrantEvent,
    ) -> Fut<'a, Grant>;

    /// Revokes a grant that is not yet revoked and writes the `revoked` event. Returns the
    /// updated grant; [`AccessError::GrantUnknown`] when there is none.
    fn revoke<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        at_millis: i64,
        event: &'a GrantEvent,
    ) -> Fut<'a, Grant>;

    /// The grant with this id.
    fn get<'a>(&'a self, id: &'a str) -> Fut<'a, Option<Grant>>;

    /// Every grant of one grantee, live or not.
    fn for_grantee<'a>(&'a self, grantee: &'a str) -> Fut<'a, Vec<Grant>>;

    /// Every grant, newest first.
    fn list<'a>(&'a self) -> Fut<'a, Vec<Grant>>;

    /// Writes an event that has no grant row (a refusal).
    fn record_event<'a>(&'a self, event: &'a GrantEvent) -> Fut<'a, ()>;
}

#[derive(Default)]
struct Inner {
    grants: Vec<Grant>,
    events: Vec<GrantEvent>,
}

/// The reference store: grants and events in memory.
#[derive(Default)]
pub struct MemoryGrantStore {
    inner: Mutex<Inner>,
}

impl MemoryGrantStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The events written so far, oldest first.
    #[must_use]
    pub fn events(&self) -> Vec<GrantEvent> {
        self.lock().events.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl GrantStore for MemoryGrantStore {
    fn insert<'a>(&'a self, grant: &'a Grant, event: &'a GrantEvent) -> Fut<'a, ()> {
        Box::pin(async move {
            let mut inner = self.lock();
            if inner.grants.iter().any(|g| g.id == grant.id) {
                return Err(AccessError::Store);
            }
            inner.grants.push(grant.clone());
            inner.events.push(event.clone());
            Ok(())
        })
    }

    fn renew<'a>(
        &'a self,
        id: &'a str,
        expires_at_millis: i64,
        event: &'a GrantEvent,
    ) -> Fut<'a, Grant> {
        Box::pin(async move {
            let mut inner = self.lock();
            let Some(grant) = inner
                .grants
                .iter_mut()
                .find(|g| g.id == id && g.revoked_at_millis.is_none())
            else {
                return Err(AccessError::GrantUnknown);
            };
            grant.expires_at_millis = expires_at_millis;
            let updated = grant.clone();
            inner.events.push(event.clone());
            Ok(updated)
        })
    }

    fn revoke<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        at_millis: i64,
        event: &'a GrantEvent,
    ) -> Fut<'a, Grant> {
        Box::pin(async move {
            let mut inner = self.lock();
            let Some(grant) = inner
                .grants
                .iter_mut()
                .find(|g| g.id == id && g.revoked_at_millis.is_none())
            else {
                return Err(AccessError::GrantUnknown);
            };
            grant.revoked_at_millis = Some(at_millis);
            grant.revoked_by = Some(by.to_owned());
            let updated = grant.clone();
            inner.events.push(event.clone());
            Ok(updated)
        })
    }

    fn get<'a>(&'a self, id: &'a str) -> Fut<'a, Option<Grant>> {
        Box::pin(async move { Ok(self.lock().grants.iter().find(|g| g.id == id).cloned()) })
    }

    fn for_grantee<'a>(&'a self, grantee: &'a str) -> Fut<'a, Vec<Grant>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .grants
                .iter()
                .filter(|g| g.grantee == grantee)
                .cloned()
                .collect())
        })
    }

    fn list<'a>(&'a self) -> Fut<'a, Vec<Grant>> {
        Box::pin(async move {
            let mut all = self.lock().grants.clone();
            all.sort_by_key(|grant| std::cmp::Reverse(grant.created_at_millis));
            Ok(all)
        })
    }

    fn record_event<'a>(&'a self, event: &'a GrantEvent) -> Fut<'a, ()> {
        Box::pin(async move {
            self.lock().events.push(event.clone());
            Ok(())
        })
    }
}
