//! [`StoreBackend`]: the admin's [`QuarantineBackend`] over the library's `QuarantineStore`
//! (`spot-quarantine-pg`'s PostgreSQL store in production, the in-memory one in tests).
//!
//! What the library already guarantees is used, not rebuilt:
//!
//! * the store is **tenant-bound** (the tenant's own database); this adapter also refuses any
//!   other tenant name, so a cross-tenant id answers as if it did not exist;
//! * a closure (`discard`) goes through `QuarantineStore::close`, which needs a `CloseAuthority`
//!   the library mints only from its sealed `QuarantineGuard`. The one a production build can
//!   name is `EngineGuard`, so a discard is checked against the permission engine **again** at
//!   the store, on the quarantined record's own DocType, whatever the admin above decided;
//! * the closing state and the closure record (who, when) are written by the store in one
//!   statement: the state and its decision record are both there or neither is.
//!
//! # Decision order
//!
//! The decision is **audited first** (`QuarantineAudit`), then the state changes. A crash between
//! the two leaves a decision with no effect, which a repeat of the action repairs; it can never
//! leave an effect with no decision. The library's audit sink is a separate seam from the store,
//! so the audit entry and the state change are not one transaction; the closure record is the
//! in-transaction part (see the task report).
//!
//! # Mapping
//!
//! A library `Held` item whose reason is `quarantine.dependent_held` is a **parked** dependent
//! here; `Released` is `Closed`. A binary (non-object) image is not returned (a size and hash
//! only is a later decision: nothing new is decided here).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use spot_core::Clock;
use spot_permissions::{PermissionEngine, Principal};
use spot_quarantine::codes::DEPENDENT_HELD;
use spot_quarantine::{
    Access, AuditAction, AuditEntry, Closure, ClosureOutcome, EngineGuard, ItemFilter, ItemId,
    QuarantineAudit, QuarantineItem, QuarantineStore, State as LibState, authorize_close,
};

use super::guards::PrincipalSource;
use super::{BackendError, Decision, Item, QuarantineBackend, State};

/// The adapter.
pub struct StoreBackend {
    tenant: String,
    store: Arc<dyn QuarantineStore>,
    audit: Arc<dyn QuarantineAudit>,
    engine: Arc<PermissionEngine>,
    principals: Arc<dyn PrincipalSource>,
    clock: Arc<dyn Clock>,
}

impl StoreBackend {
    /// An adapter for `tenant` over `store`.
    pub fn new(
        tenant: impl Into<String>,
        store: Arc<dyn QuarantineStore>,
        audit: Arc<dyn QuarantineAudit>,
        engine: Arc<PermissionEngine>,
        principals: Arc<dyn PrincipalSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            store,
            audit,
            engine,
            principals,
            clock,
        }
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }

    fn own(&self, tenant: &str) -> bool {
        tenant == self.tenant
    }

    fn view(&self, item: &QuarantineItem) -> Item {
        let state = match item.state {
            LibState::Held if item.reason_code == DEPENDENT_HELD => State::Parked,
            LibState::Held => State::Held,
            LibState::Retrying => State::Retrying,
            LibState::Released => State::Closed,
            LibState::Discarded => State::Discarded,
        };
        Item {
            id: item.id.0.to_string(),
            doctype: item.record_ref.entity.clone(),
            reason: item.reason_code.clone(),
            node: item.source.as_str().to_owned(),
            record: item.record_ref.key(),
            state,
            age_secs: self.now_micros().saturating_sub(item.first_seen) / 1_000_000,
        }
    }

    async fn load(&self, id: &str) -> Result<Option<QuarantineItem>, BackendError> {
        let Ok(number) = id.parse::<u64>() else {
            return Ok(None);
        };
        self.store
            .item(ItemId(number))
            .await
            .map_err(|_| BackendError::Unavailable)
    }
}

#[async_trait]
impl QuarantineBackend for StoreBackend {
    async fn list(
        &self,
        tenant: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Item>, BackendError> {
        if !self.own(tenant) {
            return Ok(Vec::new());
        }
        let after = after.and_then(|text| text.parse::<u64>().ok()).unwrap_or(0);
        let mut items = self
            .store
            .list(&ItemFilter::all())
            .await
            .map_err(|_| BackendError::Unavailable)?;
        items.retain(|item| item.state.is_open() && item.id.0 > after);
        items.sort_by_key(|item| item.id.0);
        Ok(items.iter().take(limit).map(|i| self.view(i)).collect())
    }

    async fn get(&self, tenant: &str, id: &str) -> Result<Option<Item>, BackendError> {
        if !self.own(tenant) {
            return Ok(None);
        }
        Ok(self.load(id).await?.map(|item| self.view(&item)))
    }

    async fn image(&self, tenant: &str, id: &str) -> Result<Option<String>, BackendError> {
        if !self.own(tenant) {
            return Ok(None);
        }
        let Ok(number) = id.parse::<u64>() else {
            return Ok(None);
        };
        let record = self
            .store
            .record(ItemId(number))
            .await
            .map_err(|_| BackendError::Unavailable)?;
        // The wire form of the record image: `{ meta_version, document: { .. fields .. }, .. }`.
        // Only the document's field object is shown (so the admin can hide a field by name); a
        // shape that is not an object is a binary image and is not returned.
        let text = record.and_then(|record| record.payload_json().ok());
        let view = text.and_then(|text| {
            let value: Value = serde_json::from_str(&text).ok()?;
            let document = value.get("document")?;
            let fields = document.get("fields").unwrap_or(document);
            fields.is_object().then(|| fields.to_string())
        });
        Ok(view)
    }

    async fn transition(
        &self,
        tenant: &str,
        id: &str,
        to: State,
        decision: Decision,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<(), BackendError> {
        if !self.own(tenant) {
            return Err(BackendError::Conflict);
        }
        let item = self.load(id).await?.ok_or(BackendError::Conflict)?;
        let now = self.now_micros();
        let action = match decision {
            Decision::Released => AuditAction::Released,
            Decision::Discarded => AuditAction::Discarded,
            Decision::Retried => AuditAction::Retried,
        };
        match to {
            State::Retrying => {
                if !item.state.can_become(LibState::Retrying) {
                    return Err(BackendError::Conflict);
                }
                self.record_decision(&item, action, actor, reason, now)
                    .await?;
                let mut next = item;
                next.state = LibState::Retrying;
                self.store
                    .update(&next)
                    .await
                    .map_err(|_| BackendError::Unavailable)
            }
            State::Discarded => {
                if !item.state.can_become(LibState::Discarded) {
                    return Err(BackendError::Conflict);
                }
                // The store's own check, on the record's DocType: a sealed guard over the real
                // engine. No principal, or a denial, closes nothing.
                let principal: Principal = self
                    .principals
                    .principal(actor)
                    .ok_or(BackendError::Conflict)?;
                let authority = authorize_close(
                    &EngineGuard::new(Arc::clone(&self.engine)),
                    &principal,
                    Access::Discard,
                    &item,
                    ClosureOutcome::Discarded,
                )
                .map_err(|_| BackendError::Conflict)?;
                self.record_decision(&item, action, actor, reason, now)
                    .await?;
                self.store
                    .close(
                        item.id,
                        Closure {
                            outcome: ClosureOutcome::Discarded,
                            by: actor.to_owned(),
                            at: now,
                        },
                        authority,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| BackendError::Unavailable)
            }
            // The admin never asks for these; refuse rather than guess.
            State::Parked | State::Held | State::Closed => Err(BackendError::Conflict),
        }
    }

    async fn summary(&self, tenant: &str) -> Result<Vec<Item>, BackendError> {
        if !self.own(tenant) {
            return Ok(Vec::new());
        }
        let items = self
            .store
            .list(&ItemFilter::all())
            .await
            .map_err(|_| BackendError::Unavailable)?;
        Ok(items.iter().map(|i| self.view(i)).collect())
    }
}

impl StoreBackend {
    async fn record_decision(
        &self,
        item: &QuarantineItem,
        action: AuditAction,
        actor: &str,
        reason: Option<&str>,
        at: i64,
    ) -> Result<(), BackendError> {
        let entry = AuditEntry {
            action,
            item: item.id,
            record_ref: item.record_ref.clone(),
            source: item.source.clone(),
            actor: actor.to_owned(),
            reason: reason.unwrap_or_default().to_owned(),
            at,
        };
        self.audit
            .record(&entry)
            .await
            .map_err(|_| BackendError::Unavailable)
    }
}
