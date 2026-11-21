//! The per-tenant recovery report from real facts: the page that answers "could I recover?".
//!
//! [`RecoveryReportService::report`] collects the two inputs that come from the object store (the
//! chain and manifest check, the snapshot listing), reads the control plane, the key service, the
//! owners' exclusions and the audit history through the crate's traits, and returns the stable
//! JSON of `spot-journal-recovery`'s assessment: verdict `Recoverable`, `NotRecoverable` or
//! `CannotConfirm`. An unreadable fact is `Fact::Unreadable`, never a default; a tenant whose
//! blob cross-check did not run **cannot be confirmed** (the crate insists, and so does this).
//! The report reads metadata and counts only.

use std::sync::Arc;

use spot_journal_audit::BlobCrossCheck;
use spot_journal_manifest::VerifyingKeys;
use spot_journal_recovery::{
    Assessment, AuditHistory, ExclusionRegistry, JournalFacts, KeyFacts, RecoveryInputs,
    Thresholds, assess, check_chain, read_snapshots,
};
use spot_journal_snapshot::SnapshotKeys;
use spot_journal_store::{JournalStore, TenantId};
use spot_journal_verify::{Expected, Mode};

use crate::error::HostError;

/// Where the blob cross-check (documents against the blob prefix) comes from, once it exists.
pub trait BlobFacts: Send + Sync {
    /// The last cross-check, or `None` when it did not run.
    fn blobs(&self) -> Option<BlobCrossCheck>;
}

/// No blob cross-check is hosted yet: the report says it cannot confirm that input.
pub struct NoBlobCheck;

impl BlobFacts for NoBlobCheck {
    fn blobs(&self) -> Option<BlobCrossCheck> {
        None
    }
}

/// Everything the report reads for one site.
pub struct ReportSources {
    /// The tenant (the site's).
    pub tenant: TenantId,
    /// The journal's source id the manifest names.
    pub source: String,
    /// The journal store.
    pub store: Arc<dyn JournalStore>,
    /// The manifest verifying keys.
    pub manifest_keys: Arc<VerifyingKeys>,
    /// The snapshot keys.
    pub snapshot_keys: Arc<dyn SnapshotKeys>,
    /// The control plane's facts: target, policy, history, lag, cadence.
    pub journal: Arc<dyn JournalFactsSync>,
    /// The key service's facts: the journal key and its backup.
    pub keys: Arc<dyn KeyFactsSync>,
    /// The exclusions, with the owners and reasons the engine registered
    /// (`Engine::capture_exclusions()`).
    pub exclusions: Arc<dyn ExclusionRegistrySync>,
    /// The audit history (the result table).
    pub history: Arc<dyn AuditHistorySync>,
    /// The blob cross-check.
    pub blobs: Arc<dyn BlobFacts>,
    /// The thresholds (the crate's defaults unless the site sets them).
    pub thresholds: Thresholds,
}

macro_rules! sync_alias {
    ($name:ident, $base:ident) => {
        /// The recovery fact trait, shareable across threads.
        pub trait $name: $base + Send + Sync {}
        impl<T: $base + Send + Sync> $name for T {}
    };
}
sync_alias!(JournalFactsSync, JournalFacts);
sync_alias!(KeyFactsSync, KeyFacts);
sync_alias!(ExclusionRegistrySync, ExclusionRegistry);
sync_alias!(AuditHistorySync, AuditHistory);

/// The service the management operation (WP-RTB-04.T1) serves.
pub struct RecoveryReportService {
    sources: ReportSources,
}

impl RecoveryReportService {
    /// A service over `sources`.
    #[must_use]
    pub fn new(sources: ReportSources) -> Self {
        Self { sources }
    }

    /// The assessment of `site` at `now_millis`. Only this service's own tenant can be asked.
    ///
    /// # Errors
    /// [`HostError::TenantMismatch`] for another site (nothing of this tenant's facts is read
    /// under another name).
    pub async fn report(
        self: &Arc<Self>,
        site: &str,
        now_millis: i64,
    ) -> Result<Assessment, HostError> {
        let tenant = self.sources.tenant.clone();
        if site != tenant.as_str() {
            return Err(HostError::TenantMismatch);
        }
        let expected = Expected {
            tenant: tenant.clone(),
            source: self.sources.source.clone(),
            generation: None,
        };
        let chain = check_chain(
            self.sources.store.as_ref(),
            &self.sources.manifest_keys,
            &expected,
            Mode::Light,
        )
        .await;
        let snapshots = read_snapshots(
            self.sources.store.as_ref(),
            tenant.as_str(),
            self.sources.snapshot_keys.as_ref(),
        )
        .await;
        let me = Arc::clone(self);
        // The fact traits are synchronous and reach the database through a worker thread: keep
        // them off the executor.
        tokio::task::spawn_blocking(move || {
            let s = &me.sources;
            let inputs = RecoveryInputs {
                tenant: s.tenant.as_str(),
                journal: &*s.journal,
                keys: &*s.keys,
                exclusions: &*s.exclusions,
                history: &*s.history,
                chain,
                snapshots,
                blobs: s.blobs.blobs(),
            };
            assess(&inputs, now_millis, &s.thresholds)
        })
        .await
        .map_err(|_| HostError::Worker)
    }
}
