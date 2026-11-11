//! The seams: who is audited, what each audit reads and where results go.
//!
//! All synchronous like the audit's blocking work, and `Send + Sync` because the job holds them
//! behind an `Arc` and runs on the blocking pool. The real implementations are the host's
//! (WP-JRN-07.T6 supplies the PostgreSQL sides); [`crate::memory`] has in-memory ones.

use std::rc::Rc;
use std::sync::Arc;

use spot_image_upcast::Registry as UpcastRegistry;
use spot_journal_audit_run::{AuditOptions, LiveSide, ScratchSide};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_snapshot::SnapshotKeys;
use spot_journal_store::{JournalStore, TenantId};

use crate::cadence::TenantAuditPlan;
use crate::error::AuditJobError;
use crate::run::TenantResult;

/// Which tenants exist, how often each is audited and when each was last audited.
pub trait TenantDirectory: Send + Sync {
    /// Every tenant with an audit schedule.
    ///
    /// # Errors
    ///
    /// [`AuditJobError::ProviderFailed`] when the directory cannot be read; the job then retries
    /// as a whole.
    fn tenants(&self) -> Result<Vec<TenantAuditPlan>, AuditJobError>;
}

/// Everything one tenant's audit reads, opened for that run only.
pub struct TenantInputs {
    /// The audit options: the tenant, the journal source, the segments, forks, derived entities
    /// and the tier's recovery time objective.
    pub options: AuditOptions<'static>,
    /// The live database, read-only.
    pub live: Box<dyn LiveSide>,
    /// The scratch store the rebuild is applied to; the runner discards it at the end.
    pub scratch: Box<dyn ScratchSide>,
    /// The journal store the rebuild reads.
    pub store: Arc<dyn JournalStore>,
    /// The tenant's public manifest keys.
    pub manifest_keys: Arc<VerifyingKeys>,
    /// The tenant's snapshot keys.
    pub snapshot_keys: Arc<dyn SnapshotKeys>,
    /// The upcasters the rebuild applies.
    pub registry: Rc<UpcastRegistry>,
}

/// Opens the inputs of one tenant's audit.
pub trait AuditInputs: Send + Sync {
    /// Opens the live side, a fresh scratch side and the journal data for `tenant`.
    ///
    /// # Errors
    ///
    /// [`AuditJobError::ProviderFailed`] when the inputs cannot be opened; that tenant's audit is
    /// then a failed result and the other tenants still run.
    fn open(&self, tenant: &TenantId) -> Result<TenantInputs, AuditJobError>;
}

/// Records the per-tenant result row and the verified position. The audit itself never writes
/// them; [`TenantResult::run`] carries the verified position of a clean run.
pub trait ResultSink: Send + Sync {
    /// Records one tenant's result. A directory that reads its last run time from what this
    /// records must return [`TenantResult::started_millis`] as `last_run_millis`.
    ///
    /// # Errors
    ///
    /// [`AuditJobError::ProviderFailed`] when the result could not be stored; the tenant is then
    /// audited again by the next call.
    fn record(&self, tenant: &TenantId, result: &TenantResult) -> Result<(), AuditJobError>;
}
