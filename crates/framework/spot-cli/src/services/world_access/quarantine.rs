//! The quarantine part of the world (WP-AI-07.T7): the real [`QuarantineOps`] over the quarantine
//! module's [`QuarantineBackend`] port.
//!
//! # What it does
//!
//! The backend is the module's own tenant-scoped, asynchronous store port (the PostgreSQL store
//! of `spot-quarantine-pg` in production). The part never touches a row, and never reads an item's
//! image: it sees ids, states and reason codes only.
//!
//! - `held` lists the open items that are parked or held (oldest first, at most
//!   [`SNAPSHOT_LIMIT`]); `item` reads one. The backend's *parked* and *held* both read as
//!   `held` (both can be retried), *closed* reads as `released` and *discarded* as `discarded`.
//! - `retry` asks the backend to move a held or parked item to retry **as the remedy's actor**
//!   (`live_as(actor)`; the decision record carries that identity) and records the decision
//!   `quarantine_retried`, exactly as the admin's own retry does. Without an actor
//!   (`live()` alone) every change is refused: the part never acts as a system principal. An item
//!   that is not held or parked, or that changed meanwhile, is refused.
//! - `restore` is **not supported by the quarantine port**: the backend has no transition back to
//!   held, and a retried item is picked up by the apply path (the seed remedy is irreversible by
//!   design). A restore to the state the item already has is a no-op.
//! - There is no reason class in the port that forbids a retry (the port's only reason rule is the
//!   *discard* permission, which this part never uses), so none is invented here; the tier
//!   authoriser and the remedy's own T3 rule are what gate it.
//! - `rehearsal` pages the open items into the in-memory fixture; a rehearsal retry never writes.
//!
//! The ops are synchronous and block on the runtime handle captured when the site was built
//! (`Handle::block_on`, never `block_in_place`).

use std::sync::Arc;

use spot_remedies::fixture::FakeQuarantine;
use spot_remedies::{
    OpError, QuarantineChange, QuarantineItem, QuarantineOps, QuarantineRef, QuarantineState,
};
use tokio::runtime::Handle;

use super::parts::{PartName, QuarantinePart, capture_runtime};
use crate::services::SiteCtx;
use crate::services::quarantine::{
    BackendError, Decision, Item, MAX_PAGE, QuarantineBackend, State,
};
use crate::services::remedies::world::WorldError;

/// The most items a listing or a snapshot reads.
pub const SNAPSHOT_LIMIT: usize = 1_000;

const NOT_SUPPORTED: &str = "not supported by the quarantine port";

/// The state of an item as the remedy ports name it.
#[must_use]
pub fn state_of(state: State) -> QuarantineState {
    match state {
        State::Parked | State::Held => QuarantineState::Held,
        State::Retrying => QuarantineState::Retrying,
        State::Closed => QuarantineState::Released,
        State::Discarded => QuarantineState::Discarded,
    }
}

/// The quarantine part over a backend, for one tenant.
pub struct BackendQuarantinePart {
    backend: Arc<dyn QuarantineBackend>,
    tenant: String,
    handle: Handle,
}

impl BackendQuarantinePart {
    /// A part over `backend` for `tenant`; `handle` is the runtime the ops block on.
    #[must_use]
    pub fn new(backend: Arc<dyn QuarantineBackend>, tenant: &str, handle: Handle) -> Self {
        Self {
            backend,
            tenant: tenant.to_owned(),
            handle,
        }
    }

    /// Up to [`SNAPSHOT_LIMIT`] open items, oldest first, page by page.
    fn open_items(&self) -> Result<Vec<Item>, BackendError> {
        let mut items: Vec<Item> = Vec::new();
        while items.len() < SNAPSHOT_LIMIT {
            let after = items.last().map(|item| item.id.clone());
            let page = self.handle.block_on(self.backend.list(
                &self.tenant,
                after.as_deref(),
                MAX_PAGE,
            ))?;
            let full = page.len() >= MAX_PAGE;
            items.extend(page);
            if !full {
                break;
            }
        }
        items.truncate(SNAPSHOT_LIMIT);
        Ok(items)
    }

    fn ops(&self, actor: Option<String>) -> BackendOps<'_> {
        BackendOps { part: self, actor }
    }
}

/// The part for a site, or `None` when no backend is provided, the tenant is empty or no runtime
/// is available to block on.
#[must_use]
pub fn from_parts(
    backend: Option<Arc<dyn QuarantineBackend>>,
    tenant: &str,
    handle: Option<Handle>,
) -> Option<Arc<dyn QuarantinePart>> {
    let backend = backend?;
    if tenant.is_empty() {
        return None;
    }
    Some(Arc::new(BackendQuarantinePart::new(
        backend, tenant, handle?,
    )))
}

/// The quarantine part for this site, or `None` when no module provides the quarantine backend
/// (today none does: the admin is assembled by the module that provides the store).
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn QuarantinePart>> {
    let env = *ctx.env().ok()?;
    from_parts(
        ctx.port::<dyn QuarantineBackend>(),
        &env.site.site,
        capture_runtime(),
    )
}

impl QuarantinePart for BackendQuarantinePart {
    fn live(&self) -> Box<dyn QuarantineOps + '_> {
        Box::new(self.ops(None))
    }

    fn live_as(&self, actor: &str) -> Box<dyn QuarantineOps + '_> {
        Box::new(self.ops(Some(actor.to_owned())))
    }

    fn rehearsal(&self) -> Result<Box<dyn QuarantineOps>, WorldError> {
        let items = self
            .open_items()
            .map_err(|_| PartName::Quarantine.rehearsal_unavailable())?;
        let mut fake = FakeQuarantine::new();
        for item in items {
            fake.seed(&item.id, state_of(item.state));
        }
        Ok(Box::new(fake))
    }
}

/// The live ops, with the identity a change is recorded under (if any).
struct BackendOps<'a> {
    part: &'a BackendQuarantinePart,
    actor: Option<String>,
}

impl QuarantineOps for BackendOps<'_> {
    fn held(&self) -> Vec<QuarantineRef> {
        self.part
            .open_items()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| state_of(item.state) == QuarantineState::Held)
                    .map(|item| QuarantineRef::new(&item.id))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn item(&self, id: &QuarantineRef) -> Option<QuarantineItem> {
        let item = self
            .part
            .handle
            .block_on(self.part.backend.get(&self.part.tenant, id.as_str()))
            .ok()??;
        Some(QuarantineItem {
            id: id.clone(),
            state: state_of(item.state),
        })
    }

    fn retry(&mut self, id: &QuarantineRef) -> Result<QuarantineChange, OpError> {
        let op = "quarantine_retry";
        let Some(actor) = self.actor.as_deref() else {
            return Err(OpError::new(op, "an acting identity is required"));
        };
        let item = self
            .part
            .handle
            .block_on(self.part.backend.get(&self.part.tenant, id.as_str()))
            .map_err(|_| OpError::new(op, "the quarantine store did not answer"))?
            .ok_or_else(|| OpError::new(op, &format!("no quarantined item `{}`", id.as_str())))?;
        let before = state_of(item.state);
        if before != QuarantineState::Held {
            return Err(OpError::new(
                op,
                &format!("item `{}` is {}, not held", id.as_str(), before.as_str()),
            ));
        }
        self.part
            .handle
            .block_on(self.part.backend.transition(
                &self.part.tenant,
                id.as_str(),
                State::Retrying,
                Decision::Retried,
                actor,
                None,
            ))
            .map_err(|error| match error {
                BackendError::Unavailable => {
                    OpError::new(op, "the quarantine store did not answer")
                }
                BackendError::Conflict => OpError::new(op, "the item changed meanwhile"),
            })?;
        Ok(QuarantineChange {
            id: id.clone(),
            before,
            after: QuarantineState::Retrying,
        })
    }

    fn restore(&mut self, change: &QuarantineChange) -> Result<(), OpError> {
        let op = "quarantine_restore";
        let item = self
            .item(&change.id)
            .ok_or_else(|| OpError::new(op, &format!("no quarantined item `{}`", change.id)))?;
        if item.state == change.before {
            return Ok(());
        }
        Err(OpError::new(op, NOT_SUPPORTED))
    }
}
