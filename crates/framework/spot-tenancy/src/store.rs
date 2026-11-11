//! The control store: the registry of tenants, their placement and their workflow journals.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{ClusterId, DatabaseId, Tenant, TenantId, TenantName, TenantState};

/// A provisioning step, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Step {
    /// Create the tenant schema (the `spot-pg-migrate` port).
    CreateSchema,
    /// Baseline migrations and seed (the `spot-cli` new-site steps).
    Seed,
    /// The tenant admin and break-glass users (IAM ports).
    CreateUsers,
    /// Issue the licence (the `spot-license` issuing port).
    IssueLicence,
    /// Register with telemetry.
    RegisterTelemetry,
    /// Flip the tenant to `Active`.
    Activate,
}

impl Step {
    /// Every step in execution order.
    pub const ALL: [Step; 6] = [
        Step::CreateSchema,
        Step::Seed,
        Step::CreateUsers,
        Step::IssueLicence,
        Step::RegisterTelemetry,
        Step::Activate,
    ];

    /// The step's fixed name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Step::CreateSchema => "create_schema",
            Step::Seed => "seed",
            Step::CreateUsers => "create_users",
            Step::IssueLicence => "issue_licence",
            Step::RegisterTelemetry => "register_telemetry",
            Step::Activate => "activate",
        }
    }
}

/// What a journal line records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalKind {
    /// The step is about to run; any resource it creates may exist from here on.
    Started,
    /// The step finished.
    Done,
    /// The step's compensation finished.
    Compensated,
}

/// One journal line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The step.
    pub step: Step,
    /// What happened.
    pub kind: JournalKind,
    /// Unix milliseconds.
    pub at: u64,
}

/// The durable registry. Implementations must make `insert_within`, `update` and `journal_append`
/// atomic, and must fail closed: a store error is an error, never a silent default.
pub trait ControlStore {
    /// Adds a tenant in `Provisioning`, enforcing name rules (a live tenant, or a deleted
    /// tenant's unexpired reservation at `now`, blocks the name), that no other live tenant holds
    /// the same cluster, database and schema, and that the placement's database does not already
    /// hold `capacity` live tenants. Returns the stored record (version 1). The name, placement
    /// and capacity checks and the insert are one atomic step (in memory under the
    /// store's lock, in PostgreSQL in one transaction that locks the placement), so two
    /// concurrent inserts against the same database can never push it past `capacity`.
    ///
    /// `capacity` is the most live tenants the placement's database may hold, taken from the
    /// placement map's cluster entry: `0` admits nothing and `u32::MAX` is the widest bound.
    ///
    /// # Errors
    /// Name, placement, id or capacity conflicts, or a store failure.
    fn insert_within(&self, tenant: Tenant, now: u64, capacity: u32) -> Result<Tenant>;
    /// Reads one tenant.
    ///
    /// # Errors
    /// [`Error::NotFound`] or a store failure.
    fn get(&self, id: &TenantId) -> Result<Tenant>;
    /// Finds the live tenant with this name, if any.
    ///
    /// # Errors
    /// A store failure.
    fn find_by_name(&self, name: &TenantName) -> Result<Option<Tenant>>;
    /// Replaces a tenant if the stored version equals `expected_version`; the stored version
    /// becomes `expected_version + 1`. The id, name and region never change; the placement
    /// changes only while the stored state is `Moving`; the state must follow the table.
    ///
    /// # Errors
    /// Version conflict, invalid transition, immutable-field change, or a store failure.
    fn update(&self, tenant: Tenant, expected_version: u64) -> Result<Tenant>;
    /// Every tenant, ordered by id.
    ///
    /// # Errors
    /// A store failure.
    fn list(&self) -> Result<Vec<Tenant>>;
    /// The number of live (non-deleted) tenants per cluster and database.
    ///
    /// # Errors
    /// A store failure.
    fn load(&self) -> Result<BTreeMap<(ClusterId, DatabaseId), u32>>;
    /// Appends one journal line for a tenant.
    ///
    /// # Errors
    /// [`Error::NotFound`] or a store failure.
    fn journal_append(&self, id: &TenantId, entry: JournalEntry) -> Result<()>;
    /// A tenant's journal, oldest first.
    ///
    /// # Errors
    /// A store failure.
    fn journal(&self, id: &TenantId) -> Result<Vec<JournalEntry>>;
}

#[derive(Default)]
struct Inner {
    tenants: BTreeMap<TenantId, Tenant>,
    journals: BTreeMap<TenantId, Vec<JournalEntry>>,
}

/// The in-memory [`ControlStore`], for tests and single-process use.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|_| Error::StoreFailed {
            reason: "the store lock is poisoned",
        })
    }
}

fn not_found(id: &TenantId) -> Error {
    Error::NotFound {
        tenant: id.to_string(),
    }
}

impl ControlStore for MemoryStore {
    fn insert_within(&self, mut tenant: Tenant, now: u64, capacity: u32) -> Result<Tenant> {
        tenant.placement.validate_for(&tenant.id)?;
        if tenant.state != TenantState::Provisioning {
            return Err(Error::WrongState {
                tenant: tenant.id.to_string(),
                state: tenant.state,
                operation: "register a tenant",
            });
        }
        let mut inner = self.lock()?;
        if inner.tenants.contains_key(&tenant.id) {
            return Err(Error::InvalidRequest {
                reason: "the tenant id is already registered",
            });
        }
        for other in inner.tenants.values() {
            if other.name == tenant.name {
                if other.state != TenantState::Deleted {
                    return Err(Error::NameTaken {
                        name: tenant.name.to_string(),
                    });
                }
                if let Some(until) = other.name_reserved_until
                    && until > now
                {
                    return Err(Error::NameReserved {
                        name: tenant.name.to_string(),
                        until,
                    });
                }
            }
            if other.state != TenantState::Deleted && other.placement == tenant.placement {
                return Err(Error::InvalidPlacement {
                    reason: "another tenant already holds this schema",
                });
            }
        }
        // The capacity check runs under the same lock as the insert, so two provisions racing
        // for the last slot cannot both win.
        let used = inner
            .tenants
            .values()
            .filter(|other| {
                other.state != TenantState::Deleted
                    && other.placement.cluster == tenant.placement.cluster
                    && other.placement.database == tenant.placement.database
            })
            .count();
        if used >= usize::try_from(capacity).unwrap_or(usize::MAX) {
            return Err(Error::CapacityExceeded {
                cluster: tenant.placement.cluster.to_string(),
                database: tenant.placement.database.to_string(),
            });
        }
        tenant.version = 1;
        tenant.retention = None;
        tenant.name_reserved_until = None;
        inner.tenants.insert(tenant.id, tenant.clone());
        inner.journals.insert(tenant.id, Vec::new());
        Ok(tenant)
    }

    fn get(&self, id: &TenantId) -> Result<Tenant> {
        self.lock()?
            .tenants
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(id))
    }

    fn find_by_name(&self, name: &TenantName) -> Result<Option<Tenant>> {
        Ok(self
            .lock()?
            .tenants
            .values()
            .find(|t| &t.name == name && t.state != TenantState::Deleted)
            .cloned())
    }

    fn update(&self, mut tenant: Tenant, expected_version: u64) -> Result<Tenant> {
        let mut inner = self.lock()?;
        let stored = inner
            .tenants
            .get(&tenant.id)
            .ok_or_else(|| not_found(&tenant.id))?;
        if stored.version != expected_version {
            return Err(Error::VersionConflict {
                tenant: tenant.id.to_string(),
                expected: expected_version,
                found: stored.version,
            });
        }
        if tenant.name != stored.name
            || tenant.region != stored.region
            || tenant.created != stored.created
        {
            return Err(Error::InvalidRequest {
                reason: "a tenant's name, region and creation time never change",
            });
        }
        if stored.state != tenant.state && !stored.state.can_become(tenant.state) {
            return Err(Error::InvalidTransition {
                tenant: tenant.id.to_string(),
                from: stored.state,
                to: tenant.state,
            });
        }
        if stored.state == TenantState::Deleted {
            return Err(Error::WrongState {
                tenant: tenant.id.to_string(),
                state: TenantState::Deleted,
                operation: "update a deleted tenant",
            });
        }
        if tenant.placement != stored.placement {
            if stored.state != TenantState::Moving {
                return Err(Error::InvalidPlacement {
                    reason: "the placement changes only while the tenant is moving",
                });
            }
            tenant.placement.validate_for(&tenant.id)?;
            if inner.tenants.values().any(|o| {
                o.id != tenant.id
                    && o.state != TenantState::Deleted
                    && o.placement == tenant.placement
            }) {
                return Err(Error::InvalidPlacement {
                    reason: "another tenant already holds this schema",
                });
            }
        }
        tenant.version = expected_version.checked_add(1).ok_or(Error::StoreFailed {
            reason: "version overflow",
        })?;
        inner.tenants.insert(tenant.id, tenant.clone());
        Ok(tenant)
    }

    fn list(&self) -> Result<Vec<Tenant>> {
        Ok(self.lock()?.tenants.values().cloned().collect())
    }

    fn load(&self) -> Result<BTreeMap<(ClusterId, DatabaseId), u32>> {
        let inner = self.lock()?;
        let mut out: BTreeMap<(ClusterId, DatabaseId), u32> = BTreeMap::new();
        for t in inner
            .tenants
            .values()
            .filter(|t| t.state != TenantState::Deleted)
        {
            let n = out
                .entry((t.placement.cluster.clone(), t.placement.database.clone()))
                .or_insert(0);
            *n = n.saturating_add(1);
        }
        Ok(out)
    }

    fn journal_append(&self, id: &TenantId, entry: JournalEntry) -> Result<()> {
        let mut inner = self.lock()?;
        inner
            .journals
            .get_mut(id)
            .ok_or_else(|| not_found(id))?
            .push(entry);
        Ok(())
    }

    fn journal(&self, id: &TenantId) -> Result<Vec<JournalEntry>> {
        self.lock()?
            .journals
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(id))
    }
}
