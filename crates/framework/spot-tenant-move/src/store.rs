//! Persisted move progress: the record, the store trait and its in-memory reference.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use spot_tenancy::TenantId;

use crate::error::{Error, Result};
use crate::plan::{MoveEvent, MoveHalt, MoveId, MovePlan, MoveState};
use crate::transfer::TransferTotals;
use crate::verify::Verification;

/// Everything known about one move, as persisted after every transition and every chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRecord {
    /// The move id, also used as the side-effect hold's migration id.
    pub id: MoveId,
    /// The plan this move executes.
    pub plan: MovePlan,
    /// The forward state, or `Aborted`.
    pub state: MoveState,
    /// The next export chunk to read; equal to the number of exported chunks.
    pub export_next: u64,
    /// The next chunk to import; equal to the number of imported chunks.
    pub import_next: u64,
    /// What the export pass consumed.
    pub exported: TransferTotals,
    /// What the import pass consumed.
    pub imported: TransferTotals,
    /// The last fingerprint comparison, once measured.
    pub verification: Option<Verification>,
    /// Why the move stopped short, if it did.
    pub halt: Option<MoveHalt>,
    /// Optimistic revision, bumped by every successful save.
    pub revision: u64,
    /// Unix milliseconds the move was started.
    pub created_at: u64,
    /// Unix milliseconds of the last transition or checkpoint.
    pub updated_at: u64,
    /// The states entered, oldest first.
    pub history: Vec<MoveEvent>,
}

impl MoveRecord {
    /// A fresh record in `Planned`.
    pub(crate) fn new(id: MoveId, plan: MovePlan, now: u64) -> Self {
        Self {
            id,
            plan,
            state: MoveState::Planned,
            export_next: 0,
            import_next: 0,
            exported: TransferTotals::default(),
            imported: TransferTotals::default(),
            verification: None,
            halt: None,
            revision: 0,
            created_at: now,
            updated_at: now,
            history: vec![MoveEvent {
                state: MoveState::Planned,
                at: now,
            }],
        }
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.plan.tenant
    }

    /// Whether the move is terminal.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Advances to the one allowed next state.
    ///
    /// # Errors
    /// [`Error::WrongState`] when `next` is not this state's successor.
    pub(crate) fn advance(&mut self, next: MoveState, now: u64) -> Result<()> {
        if !self.state.can_advance_to(next) {
            return Err(Error::WrongState {
                tenant: self.plan.tenant.to_string(),
                state: self.state,
                operation: "advance",
            });
        }
        self.enter(next, now);
        Ok(())
    }

    /// Rolls back to `Aborted` when still before cut-over.
    ///
    /// # Errors
    /// [`Error::AbortTooLate`] from `CutOver` onward.
    pub(crate) fn abort(&mut self, now: u64, reason: String) -> Result<()> {
        if !self.state.is_before_cut_over() {
            return Err(Error::AbortTooLate {
                tenant: self.plan.tenant.to_string(),
            });
        }
        self.halt = Some(MoveHalt::Aborted { reason });
        self.enter(MoveState::Aborted, now);
        Ok(())
    }

    fn enter(&mut self, state: MoveState, now: u64) {
        self.state = state;
        self.updated_at = now;
        self.history.push(MoveEvent { state, at: now });
    }
}

/// The durable move store. Implementations must make `create` and `save` atomic.
///
/// `save` is optimistic: it only writes when the stored revision still equals the
/// expected one, so two operators cannot silently overwrite each other's progress.
pub trait MoveStore: Send + Sync {
    /// Records a new move. A tenant with an existing record is refused.
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] or [`Error::StoreFailed`].
    fn create(&self, record: MoveRecord) -> Result<()>;

    /// Reads the move of `tenant`.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    fn get(&self, tenant: &TenantId) -> Result<Option<MoveRecord>>;

    /// Writes `record` when the stored revision equals `expected_revision`; the stored
    /// revision becomes `expected_revision + 1`. Returns the stored record.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] on a revision conflict or a store failure.
    fn save(&self, record: MoveRecord, expected_revision: u64) -> Result<MoveRecord>;
}

/// The reference in-memory store. It is the contract the PostgreSQL control store must
/// satisfy in a later task.
#[derive(Debug, Default)]
pub struct MemoryMoveStore {
    records: Mutex<BTreeMap<TenantId, MoveRecord>>,
}

impl MemoryMoveStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<TenantId, MoveRecord>>> {
        self.records.lock().map_err(|_| Error::StoreFailed {
            tenant: String::new(),
            reason: "the in-memory store is poisoned",
        })
    }
}

impl MoveStore for MemoryMoveStore {
    fn create(&self, mut record: MoveRecord) -> Result<()> {
        let tenant = record.plan.tenant;
        let mut records = self.lock()?;
        if records.contains_key(&tenant) {
            return Err(Error::AlreadyExists {
                tenant: tenant.to_string(),
            });
        }
        record.revision = 0;
        records.insert(tenant, record);
        Ok(())
    }

    fn get(&self, tenant: &TenantId) -> Result<Option<MoveRecord>> {
        Ok(self.lock()?.get(tenant).cloned())
    }

    fn save(&self, mut record: MoveRecord, expected_revision: u64) -> Result<MoveRecord> {
        let tenant = record.plan.tenant;
        let mut records = self.lock()?;
        let Some(stored) = records.get(&tenant) else {
            return Err(Error::StoreFailed {
                tenant: tenant.to_string(),
                reason: "the move disappeared from the store",
            });
        };
        if stored.revision != expected_revision {
            return Err(Error::StoreFailed {
                tenant: tenant.to_string(),
                reason: "the move changed since it was read",
            });
        }
        record.revision = expected_revision.saturating_add(1);
        let saved = record.clone();
        records.insert(tenant, record);
        Ok(saved)
    }
}
