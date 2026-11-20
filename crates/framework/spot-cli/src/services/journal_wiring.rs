//! The journal wiring: the providers the hosted journal services were written without
//! (WP-RTB-01.T9, ADR-061 sections 4, 8 and 9). Operator guide:
//! `docs/running-spot/journal_wiring.md`.
//!
//! Only on a site whose journal is on (`[services.journal]` has a target, `[services.secrets]`
//! exists) it provides: a read-only `dyn JournalStore` at the journal target and the
//! `dyn ManifestKeys` derived from the site's key provider (`journal_access`); `dyn AuditWiring`
//! (`journal_audit`); `dyn JournalOps` and `dyn SyncOps` (`mgmt_exec`). A site without a journal
//! keeps the `501` answers and registers neither the audit nor the executors.
//!
//! [`ORDER`] 1105 sits after `journal` (1100) and before the three consumers (1210, 1220, 1230),
//! which ask for the ports while they register. The shipper's handle, the audit service and the
//! sync service exist only after every module registered, so the providers pick them up in a
//! finaliser and answer `Unavailable` until then. The shipper alone writes the journal: the store
//! handed out refuses every write and delete.
//!
//! Not served, with a catalogued refusal naming the missing piece: `journal.snapshot.take`,
//! `journal.reship`, `journal.policy.set`, `journal.rebuild.*`, `journal.reader.*` and
//! `journal.subject.erase` (each needs a hook in a module outside this task). Settings:
//! `[services.journal_wiring]` `enabled` (default true) and `rto_hours` (default 4).

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use serde::Deserialize;
use spot_document::engine::ExclusionKind;
use spot_journal_audit::{
    DerivedEntities, DurabilityPolicy, ExclusionReason, JournalTarget, KeyBackupStatus,
};
use spot_journal_audit_host_pg::{
    AllWeekly, AuditHistorySync, JournalData, JournalSource, NoBlobCheck, ReplayPolicies,
    ReportSources,
};
use spot_journal_audit_run::AuditOptions;
use spot_journal_manifest::{SignedManifest, VerifyingKeys, verify};
use spot_journal_rebuild_plan::{ForkInfo, SegmentRange};
use spot_journal_recovery::{
    Cadence, ExclusionEntry, ExclusionRegistry, FactResult, FactUnreadable, JournalFacts, KeyFacts,
    Thresholds,
};
use spot_journal_snapshot::{SnapshotKeys, list_snapshots};
use spot_journal_snapshot_pg::SnapshotKeyring;
use spot_journal_store::{
    FolderStore, JournalStore, Key, Layout, ObjectEntry, ObjectMeta, StoreError, StoreFuture,
    StoredObject, TenantId,
};
use spot_journal_verify::{Expected, Mode, verify_tenant};
use spot_mgmt_journal::args::{
    OnUnreachable as WireUnreachable, PolicySetting, Position, ReaderGrantArgs, ReaderRevokeArgs,
    RebuildPlanArgs, RebuildRunArgs, SnapshotListArgs, SubjectEraseArgs,
};
use spot_mgmt_journal::results::{
    AuditOutcome, AuditResult, JournalState, KeyBackupState, PolicyResult, ReaderGrantResult,
    ReaderListResult, ReaderRevokeResult, RebuildPlanResult, RebuildRunResult,
    RecoveryReportResult, ReshipResult, SnapshotInfo, SnapshotListResult, SnapshotTakeResult,
    StatusResult, SubjectEraseResult, VerifyResult,
};
use spot_mgmt_journal::{ExecError as JournalError, ExecKind as JournalKind, JournalOps};
use spot_mgmt_sync::args::{
    EnrolArgs, NodeArgs, PolicyShowArgs, QuarantineDiscardArgs, QuarantineRetryArgs, ResendArgs,
    RoleChangeArgs,
};
use spot_sync_health::HealthView;
use spot_mgmt_sync::results::{
    BootstrapStatusResult, ConditionListResult, EnrolResult, NodeListResult,
    NodeShowResult, NodeStateResult, NotifyContactResult, PolicyLintResult, PolicyShowResult,
    QuarantineDiscardResult, QuarantineListResult, QuarantineRetryResult, ResendResult,
    RoleChangeResult, RotateCertificateResult,
};
use spot_mgmt_sync::{ExecError as SyncError, ExecKind as SyncKind, SyncOps};
use spot_secrets::KeyProvider;
use tokio::runtime::Handle;

use crate::live::SiteLive;
use crate::services::SiteCtx;
use crate::services::journal::{
    self, JournalHandle, JournalKeys, OnUnreachable, PolicyName, check_folder_target,
};
use crate::services::journal_access::ManifestKeys;
use crate::services::journal_audit::{AuditService, AuditWiring, RunNowError};
use crate::services::secrets::SecretsHost;
use crate::{Error, Result};

/// Start order: after the journal service (1100), before the three modules that consume what this
/// one provides (1210, 1220, 1230).
pub const ORDER: u32 = 1105;

/// `[services.journal_wiring]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` provides nothing: the access routes answer `501` and the audit and the executors
    /// stay unregistered.
    pub enabled: bool,
    /// The recovery time objective an audit judges a rebuild against, in hours (1 to 720).
    pub rto_hours: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            rto_hours: 4,
        }
    }
}

// The read-only store.

/// The journal folder, opened for reading only, checked on every use.
pub struct FolderReader {
    target: PathBuf,
    forbidden: Vec<PathBuf>,
}

impl FolderReader {
    /// A reader of the folder at `target`; `forbidden` are the directories the journal must not
    /// live in.
    #[must_use]
    pub fn new(target: PathBuf, forbidden: Vec<PathBuf>) -> Self {
        Self { target, forbidden }
    }

    fn open(&self) -> std::result::Result<FolderStore, StoreError> {
        let canonical = check_folder_target(&self.target, &self.forbidden).map_err(|e| {
            StoreError::Backend {
                message: e.code().to_owned(),
            }
        })?;
        FolderStore::new(canonical)
    }
}

#[rustfmt::skip]
impl JournalStore for FolderReader {
    fn put_new<'a>(&'a self, _key: &'a str, _bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async { Err(StoreError::Backend { message: "journal_wiring.read_only".to_owned() }) })
    }
    fn put_replace<'a>(&'a self, key: &'a str, _bytes: Vec<u8>, _etag: &'a str) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move { Err(StoreError::ReplaceRefused { key: key.to_owned() }) })
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        Box::pin(async move { self.open()?.get(key).await })
    }
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        Box::pin(async move { self.open()?.head(key).await })
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> StoreFuture<'a, Vec<ObjectEntry>> {
        Box::pin(async move { self.open()?.list(prefix, after, limit).await })
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move { Err(StoreError::DeleteRefused { key: key.to_owned() }) })
    }
}

/// The manifest verifying keys derived from the site's key provider.
pub struct WiringKeys(VerifyingKeys);

impl ManifestKeys for WiringKeys {
    fn keys(&self) -> std::result::Result<VerifyingKeys, String> {
        Ok(self.0.clone())
    }
}

// What is only known after every module registered.

/// The sync host's implementation of the sync operations. A port the sync services provide; this
/// module's [`SyncOps`] delegates to it and refuses while it is absent.
pub trait SyncService: SyncOps {}

/// One entity the owners excluded from capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExclusionRow {
    /// The DocType (storage name).
    pub doctype: String,
    /// The owner that excluded it.
    pub owner: String,
    /// Derived or local-only.
    pub kind: ExclusionKind,
}

/// Where the exclusions come from: the site's running document engine, read each time. `None`
/// while the engine is not running (a fact the report then calls unreadable).
pub trait CaptureExclusions: Send + Sync {
    /// The exclusions in force now.
    fn current(&self) -> Option<Vec<ExclusionRow>>;
}

impl CaptureExclusions for SiteLive {
    fn current(&self) -> Option<Vec<ExclusionRow>> {
        let engine = self.engine()?;
        Some(
            engine
                .capture_exclusions()
                .iter()
                .map(|e| ExclusionRow {
                    doctype: e.doctype.clone(),
                    owner: e.owner.clone(),
                    kind: e.kind,
                })
                .collect(),
        )
    }
}

/// Handles picked up in the finaliser.
#[derive(Default)]
struct Late {
    journal: OnceLock<Arc<JournalHandle>>,
    audit: OnceLock<Arc<AuditService>>,
    sync: OnceLock<Arc<dyn SyncService>>,
}

/// What every provider of one site shares.
struct Site {
    site: String,
    tenant: TenantId,
    policy: PolicySetting,
    region: String,
    store: Arc<FolderReader>,
    manifest_keys: Arc<VerifyingKeys>,
    snapshot_keys: Arc<dyn SnapshotKeys>,
    exclusions: Arc<dyn CaptureExclusions>,
    catalog: Arc<spot_storage_contract::Catalog>,
    rto_millis: u64,
    late: Late,
}

// The audit's journal data: segments and forks from the verified manifests.

/// What the manifests of every generation say: the sealed segments and the recorded forks.
pub async fn read_catalogue(
    store: &dyn JournalStore,
    keys: &VerifyingKeys,
    tenant: &str,
) -> std::result::Result<(Vec<SegmentRange>, Vec<ForkInfo>), &'static str> {
    let prefix = format!("{tenant}/manifest/");
    let entries = store
        .list(&prefix, None, 1000)
        .await
        .map_err(|_| "journal_wiring.store_unreadable")?;
    let mut segments = Vec::new();
    let mut forks: Vec<ForkInfo> = Vec::new();
    for entry in entries {
        let Ok(Key::Manifest(found)) = Layout::parse(&entry.key) else {
            continue;
        };
        let generation = found.generation;
        let key = Layout::manifest(tenant, generation)
            .map_err(|_| "journal_wiring.manifest_key")?
            .to_key();
        let Some(object) = store
            .get(&key)
            .await
            .map_err(|_| "journal_wiring.store_unreadable")?
        else {
            continue;
        };
        let signed = SignedManifest::from_bytes(&object.bytes)
            .map_err(|_| "journal_wiring.manifest_invalid")?;
        // A manifest that does not verify is never trusted for an audit, and the audit does not
        // run on a guess: the whole read fails.
        let verified = verify(&signed, keys).map_err(|_| "journal_wiring.manifest_unverified")?;
        if verified.generation() != generation || verified.source() != tenant {
            return Err("journal_wiring.manifest_mismatch");
        }
        for segment in verified.segments() {
            segments.push(SegmentRange {
                generation: segment.generation,
                first_seq: segment.first_seq,
                last_seq: segment.last_seq,
            });
        }
        for fork in verified.forks() {
            let info = ForkInfo {
                generation: fork.generation,
                from_position: fork.forked_from_position,
            };
            if !forks.contains(&info) {
                forks.push(info);
            }
        }
    }
    segments.sort_by_key(|s| (s.generation, s.first_seq));
    forks.sort_by_key(|f| f.generation);
    Ok((segments, forks))
}

/// The journal data of one audit run, opened on the blocking pool the audit runs on.
struct SiteJournalSource {
    shared: Arc<Site>,
    runtime: Handle,
}

impl JournalSource for SiteJournalSource {
    fn open(
        &self,
        tenant: &TenantId,
    ) -> std::result::Result<JournalData, spot_journal_audit_host_pg::HostError> {
        use spot_journal_audit_host_pg::HostError;
        let site = &self.shared;
        if tenant != &site.tenant {
            return Err(HostError::TenantMismatch);
        }
        let (segments, forks) = self
            .runtime
            .block_on(read_catalogue(
                site.store.as_ref(),
                &site.manifest_keys,
                tenant.as_str(),
            ))
            .map_err(|_| HostError::Source)?;
        let mut options = AuditOptions::new(
            tenant.clone(),
            site.site.clone(),
            1,
            segments,
            site.rto_millis,
        );
        options.forks = forks;
        // Entities the owners excluded from capture are not in the rebuilt side, so the compare
        // leaves them out, derived or local-only alike.
        let derived = DerivedEntities::from_names(
            site.exclusions
                .current()
                .unwrap_or_default()
                .into_iter()
                .map(|row| row.doctype),
        );
        options.derived = derived.clone();
        Ok(JournalData {
            options,
            store: Arc::clone(&site.store) as Arc<dyn JournalStore>,
            manifest_keys: Arc::clone(&site.manifest_keys),
            snapshot_keys: Arc::clone(&site.snapshot_keys),
            registry: Rc::new(spot_image_upcast::Registry::new()),
            catalog: Arc::clone(&site.catalog),
            derived,
            // The apps register financial fingerprints later; none means the compare covers
            // records only, and the report says so.
            fingerprints: Vec::new(),
        })
    }
}

/// The recovery report's control-plane facts, read from the settings and the shipper's progress.
struct SiteJournalFacts {
    shared: Arc<Site>,
}

impl JournalFacts for SiteJournalFacts {
    fn journal_target(&self) -> FactResult<Option<JournalTarget>> {
        // The folder's path is not told: kind and region only.
        Ok(Some(JournalTarget {
            target: "folder".to_owned(),
            region: self.shared.region.clone(),
        }))
    }

    fn durability_policy(&self) -> FactResult<Option<DurabilityPolicy>> {
        Ok(Some(match self.shared.policy {
            PolicySetting::Local => DurabilityPolicy::Local,
            PolicySetting::ReplicaSync => DurabilityPolicy::ReplicaSync,
            PolicySetting::RemoteSync { .. } => DurabilityPolicy::RemoteSync,
        }))
    }

    fn has_offsite_history(&self) -> FactResult<bool> {
        let handle = self
            .shared
            .late
            .journal
            .get()
            .ok_or_else(|| FactUnreadable::new("offsite_history"))?;
        let progress = handle.progress().now();
        Ok(progress.durable.seq > 0 || progress.shipped_total > 0)
    }

    fn tail_lag_millis(&self) -> FactResult<Option<i64>> {
        let handle = self
            .shared
            .late
            .journal
            .get()
            .ok_or_else(|| FactUnreadable::new("tail_lag"))?;
        let lag = i64::try_from(handle.progress().now().lag_secs).unwrap_or(i64::MAX);
        Ok(Some(lag.saturating_mul(1000)))
    }

    fn audit_cadence(&self) -> FactResult<Cadence> {
        // The directory runs every tenant weekly (`AllWeekly`), so the report judges it so.
        Ok(Cadence::Weekly)
    }
}

/// The journal key is derived from the provider when the site starts: if this module registered,
/// the derivation worked. The backup is not hosted, so it is `Unverified`, never `Confirmed`.
struct SiteKeyFacts;

impl KeyFacts for SiteKeyFacts {
    fn journal_key_present(&self) -> FactResult<bool> {
        Ok(true)
    }

    fn key_backup(&self) -> FactResult<KeyBackupStatus> {
        Ok(KeyBackupStatus::Unverified)
    }
}

/// The owners' exclusions, read from the running document engine each time.
struct SiteExclusions {
    shared: Arc<Site>,
}

impl ExclusionRegistry for SiteExclusions {
    fn exclusions(&self) -> FactResult<Vec<ExclusionEntry>> {
        let rows = self
            .shared
            .exclusions
            .current()
            .ok_or_else(|| FactUnreadable::new("exclusions"))?;
        Ok(rows
            .into_iter()
            .map(|row| {
                ExclusionEntry::new(
                    row.doctype,
                    row.owner,
                    match row.kind {
                        ExclusionKind::Derived => ExclusionReason::Derived,
                        ExclusionKind::Local => ExclusionReason::LocalOnly,
                    },
                )
            })
            .collect())
    }
}

/// The `AuditWiring` handed out as a port: it owns the shared site state.
struct Wired {
    shared: Arc<Site>,
    runtime: Handle,
}

impl AuditWiring for Wired {
    fn tenant(&self) -> TenantId {
        self.shared.tenant.clone()
    }

    fn source(&self) -> Arc<dyn JournalSource> {
        Arc::new(SiteJournalSource {
            shared: Arc::clone(&self.shared),
            runtime: self.runtime.clone(),
        })
    }

    fn policies(&self) -> Arc<dyn ReplayPolicies> {
        Arc::new(AllWeekly)
    }

    fn report_sources(&self, history: Arc<dyn AuditHistorySync>) -> ReportSources {
        let site = &self.shared;
        ReportSources {
            tenant: site.tenant.clone(),
            source: site.site.clone(),
            store: Arc::clone(&site.store) as Arc<dyn JournalStore>,
            manifest_keys: Arc::clone(&site.manifest_keys),
            snapshot_keys: Arc::clone(&site.snapshot_keys),
            journal: Arc::new(SiteJournalFacts {
                shared: Arc::clone(site),
            }),
            keys: Arc::new(SiteKeyFacts),
            exclusions: Arc::new(SiteExclusions {
                shared: Arc::clone(site),
            }),
            history,
            // No blob cross-check is hosted: the report says it cannot confirm that input.
            blobs: Arc::new(NoBlobCheck),
            thresholds: Thresholds::default(),
        }
    }
}

// The journal operations.

fn unavailable<T>(reason: &str) -> std::result::Result<T, JournalError> {
    Err(JournalError::new(JournalKind::Unavailable, reason))
}

/// The service-side implementation of the journal operations. It is not bound to a caller:
/// `mgmt_exec` wraps it with the permission check, the audit and the limits of the signed command.
pub struct JournalOpsHost {
    shared: Arc<Site>,
}

impl JournalOpsHost {
    fn journal(&self) -> std::result::Result<&Arc<JournalHandle>, JournalError> {
        self.shared.late.journal.get().ok_or_else(|| {
            JournalError::new(JournalKind::Unavailable, "journal_wiring.journal_not_ready")
        })
    }

    fn audit(&self) -> std::result::Result<&Arc<AuditService>, JournalError> {
        self.shared.late.audit.get().ok_or_else(|| {
            JournalError::new(JournalKind::Unavailable, "journal_wiring.audit_not_running")
        })
    }
}

fn wire_position(generation: u64, seq: u64) -> Position {
    Position { generation, seq }
}

fn snapshot_id(generation: u64, position: u64) -> String {
    format!("g{generation}-s{position}")
}

// Refusals (each a catalogued reason, see the guide): snapshot_take (the snapshot service has no
// take-now call), policy_set (the policy is configuration), reship (the shipper resumes from its
// watermark by itself), rebuild_* (offline, WP-RTB-01.T10), reader_* (the grant service is not a
// port), subject_erase (the eraser needs the requesting user, which this host is not bound to).
type J<T> = std::result::Result<T, JournalError>;

#[async_trait]
#[rustfmt::skip]
impl JournalOps for JournalOpsHost {
    async fn status(&self) -> std::result::Result<StatusResult, JournalError> {
        let handle = self.journal()?;
        let progress = handle.progress().now();
        let state = if !handle.enabled() {
            JournalState::NoTarget
        } else if progress.degraded || progress.stopped.is_some() {
            if handle.on_unreachable() == OnUnreachable::Halt
                && handle.policy() == PolicyName::RemoteSync
            {
                JournalState::Halted
            } else {
                JournalState::Degraded
            }
        } else {
            JournalState::Ok
        };
        Ok(StatusResult {
            state,
            // The local outbox head is the durability gate's to know, not the shipper's.
            head: None,
            shipped: (progress.durable.seq > 0)
                .then(|| wire_position(progress.durable.generation, progress.durable.seq)),
            // A segment is sealed straight into the store: none waits to be shipped.
            unshipped_segments: 0,
            policy: self.shared.policy,
        })
    }

    async fn recovery_report(&self) -> std::result::Result<RecoveryReportResult, JournalError> {
        use spot_journal_recovery::Fact;
        let assessment = self
            .audit()?
            .report(&self.shared.site)
            .await
            .map_err(|_| JournalError::new(JournalKind::Unavailable, "journal_wiring.report_unavailable"))?;
        let report = assessment.report;
        let (target_configured, region) = match &report.journal {
            Fact::Known(Some(target)) => (
                true,
                Some(target.region.clone()).filter(|region| !region.is_empty()),
            ),
            _ => (false, None),
        };
        let (last_verified, last_verified_at) = match &report.last_verified {
            Fact::Known(Some(v)) => (
                Some(wire_position(v.position.generation, v.position.seq)),
                Some(spot_journal_access::rfc3339(v.at_millis)),
            ),
            _ => (None, None),
        };
        // A key backup that cannot be shown to be current is reported as missing: the report errs
        // towards the worse answer.
        let key_backup = match &report.key_backup {
            Fact::Known(KeyBackupStatus::Confirmed { .. }) => KeyBackupState::Current,
            _ => KeyBackupState::Missing,
        };
        let Fact::Known(excluded) = &report.excluded_entities else {
            return unavailable("journal_wiring.exclusions_unreadable");
        };
        Ok(RecoveryReportResult {
            target_configured,
            region,
            policy: self.shared.policy,
            last_verified,
            last_verified_at,
            oldest_snapshot: report
                .oldest_snapshot
                .map(|o| snapshot_id(o.position.generation, o.position.seq)),
            key_backup,
            excluded_entities: u64::try_from(excluded.len()).unwrap_or(u64::MAX),
        })
    }

    async fn snapshot_list(
        &self,
        args: SnapshotListArgs,
    ) -> std::result::Result<SnapshotListResult, JournalError> {
        let site = &self.shared;
        let mut listed = list_snapshots(site.store.as_ref(), &site.site, &*site.snapshot_keys)
            .await
            .map_err(|_| JournalError::new(JournalKind::Unavailable, "journal_wiring.store_unreadable"))?;
        listed.sort_by_key(|s| std::cmp::Reverse((s.generation, s.position)));
        let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
        Ok(SnapshotListResult {
            snapshots: listed
                .into_iter()
                .take(limit)
                .map(|info| SnapshotInfo {
                    id: snapshot_id(info.generation, info.position),
                    position: wire_position(info.generation, info.position),
                    taken_at: spot_journal_access::rfc3339(info.taken_at_millis),
                    // The listing reads the index, not the parts: the size is not known here.
                    bytes: 0,
                })
                .collect(),
        })
    }

    async fn snapshot_take(&self) -> J<SnapshotTakeResult> { unavailable("journal_wiring.snapshot_take_not_hosted") }

    async fn verify(&self) -> std::result::Result<VerifyResult, JournalError> {
        let site = &self.shared;
        let expected = Expected {
            tenant: site.tenant.clone(),
            source: site.site.clone(),
            generation: None,
        };
        let report = verify_tenant(
            site.store.as_ref(),
            &site.manifest_keys,
            &expected,
            Mode::Deep,
        )
        .await
        .map_err(|_| JournalError::new(JournalKind::Failed, "journal_wiring.verify_failed"))?;
        let first = report.findings.first();
        Ok(VerifyResult {
            // A run that stopped at its budget has not looked at everything: it is not "ok".
            ok: report.findings.is_empty() && !report.partial,
            segments_checked: u64::try_from(report.segments_examined).unwrap_or(u64::MAX),
            first_failure: first.map(|f| wire_position(f.generation, f.first_seq)),
            reason: first.map(|f| f.code().to_owned()),
        })
    }

    async fn audit_last(&self) -> std::result::Result<AuditResult, JournalError> {
        use spot_journal_recovery::Fact;
        let assessment = self
            .audit()?
            .report(&self.shared.site)
            .await
            .map_err(|_| JournalError::new(JournalKind::Unavailable, "journal_wiring.report_unavailable"))?;
        let (Fact::Known(verified), Fact::Known(failure)) = (
            &assessment.report.last_verified,
            &assessment.report.last_failure,
        ) else {
            return unavailable("journal_wiring.audit_history_unreadable");
        };
        let (outcome, at) = match (verified, failure) {
            (_, Some(f)) if verified.is_none_or(|v| f.at_millis >= v.at_millis) => {
                (AuditOutcome::Mismatch, Some(f.at_millis))
            }
            (Some(v), _) => (AuditOutcome::Match, Some(v.at_millis)),
            _ => (AuditOutcome::NeverRun, None),
        };
        Ok(AuditResult {
            outcome,
            ran_at: at.map(spot_journal_access::rfc3339),
            // The history keeps the verdict and the position, not the count.
            records_compared: 0,
            first_difference: None,
            reason: None,
        })
    }

    async fn audit_run(&self) -> std::result::Result<AuditResult, JournalError> {
        use spot_journal_audit_job::TenantOutcome;
        let site = &self.shared;
        let summary = self
            .audit()?
            .run_now(&site.site)
            .await
            .map_err(|error| match error {
                RunNowError::WrongSite => JournalError::new(JournalKind::NotFound, "journal_wiring.wrong_site"),
                RunNowError::RateLimited(_) => {
                    JournalError::new(JournalKind::Conflict, "journal_wiring.audit_rate_limited")
                }
                RunNowError::Busy => JournalError::new(JournalKind::Conflict, "journal_wiring.audit_busy"),
                RunNowError::Failed(_) => JournalError::new(JournalKind::Failed, "journal_wiring.audit_failed"),
            })?;
        let Some(result) = summary.result(site.tenant.as_str()) else {
            return Err(JournalError::new(JournalKind::Failed, "journal_wiring.audit_not_run"));
        };
        let (outcome, reason) = match result.outcome {
            TenantOutcome::Clean => (AuditOutcome::Match, None),
            TenantOutcome::Mismatch => (AuditOutcome::Mismatch, None),
            TenantOutcome::Failed(info) => (AuditOutcome::Error, Some(info.code.to_owned())),
        };
        Ok(AuditResult {
            outcome,
            ran_at: Some(spot_journal_access::rfc3339(result.finished_millis)),
            records_compared: 0,
            first_difference: None,
            reason,
        })
    }

    async fn policy_get(&self) -> J<PolicyResult> { Ok(PolicyResult { policy: self.shared.policy, previous: None }) }

    async fn policy_set(&self, _p: PolicySetting) -> J<PolicyResult> { Err(JournalError::new(JournalKind::Conflict, "journal_wiring.policy_is_site_configuration")) }

    async fn reship(&self) -> J<ReshipResult> { unavailable("journal_wiring.reship_not_hosted") }

    async fn rebuild_plan(&self, _a: RebuildPlanArgs) -> J<RebuildPlanResult> { unavailable("journal_wiring.rebuild_is_offline") }

    async fn rebuild_run(&self, _a: RebuildRunArgs) -> J<RebuildRunResult> { unavailable("journal_wiring.rebuild_is_offline") }

    async fn reader_list(&self) -> J<ReaderListResult> { unavailable("journal_wiring.reader_grants_not_wired") }

    async fn reader_grant(&self, _a: ReaderGrantArgs) -> J<ReaderGrantResult> { unavailable("journal_wiring.reader_grants_not_wired") }

    async fn reader_revoke(&self, _a: ReaderRevokeArgs) -> J<ReaderRevokeResult> { unavailable("journal_wiring.reader_grants_not_wired") }

    async fn subject_erase(&self, _a: SubjectEraseArgs) -> J<SubjectEraseResult> { unavailable("journal_wiring.erase_needs_caller") }
}

// The sync operations: delegation to the sync host.

/// The sync operations of this site. Each one is the sync host's own ([`SyncService`]); while no
/// sync service is provided each refuses with `sync_service_absent`.
pub struct SyncOpsHost {
    shared: Arc<Site>,
}

impl SyncOpsHost {
    fn service(&self) -> std::result::Result<&Arc<dyn SyncService>, SyncError> {
        self.shared.late.sync.get().ok_or_else(|| {
            SyncError::new(SyncKind::Unavailable, "journal_wiring.sync_service_absent")
        })
    }
}

type S<T> = std::result::Result<T, SyncError>;

#[async_trait]
#[rustfmt::skip]
impl SyncOps for SyncOpsHost {
    async fn health(&self) -> S<HealthView> { self.service()?.health().await }
    async fn node_list(&self) -> S<NodeListResult> { self.service()?.node_list().await }
    async fn node_show(&self, a: NodeArgs) -> S<NodeShowResult> { self.service()?.node_show(a).await }
    async fn node_enrol(&self, a: EnrolArgs) -> S<EnrolResult> { self.service()?.node_enrol(a).await }
    async fn node_suspend(&self, a: NodeArgs) -> S<NodeStateResult> { self.service()?.node_suspend(a).await }
    async fn node_resume(&self, a: NodeArgs) -> S<NodeStateResult> { self.service()?.node_resume(a).await }
    async fn node_revoke(&self, a: NodeArgs) -> S<NodeStateResult> { self.service()?.node_revoke(a).await }
    async fn node_rotate_certificate(&self, a: NodeArgs) -> S<RotateCertificateResult> { self.service()?.node_rotate_certificate(a).await }
    async fn node_resend(&self, a: ResendArgs) -> S<ResendResult> { self.service()?.node_resend(a).await }
    async fn node_notify_contact(&self, a: NodeArgs) -> S<NotifyContactResult> { self.service()?.node_notify_contact(a).await }
    async fn condition_list(&self) -> S<ConditionListResult> { self.service()?.condition_list().await }
    async fn quarantine_list(&self) -> S<QuarantineListResult> { self.service()?.quarantine_list().await }
    async fn quarantine_retry(&self, a: QuarantineRetryArgs) -> S<QuarantineRetryResult> { self.service()?.quarantine_retry(a).await }
    async fn quarantine_discard(&self, a: QuarantineDiscardArgs) -> S<QuarantineDiscardResult> { self.service()?.quarantine_discard(a).await }
    async fn policy_show(&self, a: PolicyShowArgs) -> S<PolicyShowResult> { self.service()?.policy_show(a).await }
    async fn policy_lint(&self) -> S<PolicyLintResult> { self.service()?.policy_lint().await }
    async fn role_change(&self, a: RoleChangeArgs) -> S<RoleChangeResult> { self.service()?.role_change(a).await }
    async fn bootstrap_status(&self) -> S<BootstrapStatusResult> { self.service()?.bootstrap_status().await }
}

// Assembly and registration.

/// What a site's wiring is built from.
pub struct Parts {
    /// The site name (also the journal tenant and source).
    pub site: String,
    /// The policy in force (from `[services.journal]`).
    pub policy: PolicySetting,
    /// The region text for the report.
    pub region: String,
    /// The journal folder.
    pub target: PathBuf,
    /// Directories the journal must not live in.
    pub forbidden: Vec<PathBuf>,
    /// The manifest verifying keys.
    pub manifest_keys: VerifyingKeys,
    /// The snapshot verifying keys.
    pub snapshot_keys: Arc<dyn SnapshotKeys>,
    /// The owners' capture exclusions.
    pub exclusions: Arc<dyn CaptureExclusions>,
    /// The site's catalogue.
    pub catalog: Arc<spot_storage_contract::Catalog>,
    /// The recovery time objective, in hours.
    pub rto_hours: u64,
    /// The runtime the audit's blocking threads re-enter.
    pub runtime: Handle,
}

/// Everything one site provides.
#[derive(Clone)]
pub struct Wiring {
    /// The read-only journal store (`journal_access`).
    pub store: Arc<dyn JournalStore>,
    /// The manifest keys (`journal_access`).
    pub keys: Arc<dyn ManifestKeys>,
    /// The audit's wiring (`journal_audit`).
    pub audit: Arc<dyn AuditWiring>,
    /// The journal operations (`mgmt_exec`).
    pub journal_ops: Arc<dyn JournalOps>,
    /// The sync operations (`mgmt_exec`).
    pub sync_ops: Arc<dyn SyncOps>,
    shared: Arc<Site>,
}

impl Wiring {
    /// Builds the providers of one site.
    ///
    /// # Errors
    /// The site name is not a usable journal tenant.
    pub fn new(parts: Parts) -> Result<Self> {
        let tenant = TenantId::new(&parts.site).map_err(|_| {
            Error(format!(
                "journal_wiring: `{}` is not a journal tenant",
                parts.site
            ))
        })?;
        let shared = Arc::new(Site {
            site: parts.site,
            tenant,
            policy: parts.policy,
            region: parts.region,
            store: Arc::new(FolderReader::new(parts.target, parts.forbidden)),
            manifest_keys: Arc::new(parts.manifest_keys.clone()),
            snapshot_keys: parts.snapshot_keys,
            exclusions: parts.exclusions,
            catalog: parts.catalog,
            rto_millis: parts.rto_hours.clamp(1, 720).saturating_mul(3_600_000),
            late: Late::default(),
        });
        Ok(Self {
            store: Arc::clone(&shared.store) as Arc<dyn JournalStore>,
            keys: Arc::new(WiringKeys(parts.manifest_keys)),
            audit: Arc::new(Wired {
                shared: Arc::clone(&shared),
                runtime: parts.runtime,
            }),
            journal_ops: Arc::new(JournalOpsHost {
                shared: Arc::clone(&shared),
            }),
            sync_ops: Arc::new(SyncOpsHost {
                shared: Arc::clone(&shared),
            }),
            shared,
        })
    }

    /// Hands the shipper's handle to the providers (set once; a second call changes nothing).
    pub fn bind_journal(&self, handle: Arc<JournalHandle>) {
        let _ = self.shared.late.journal.set(handle);
    }

    /// Hands the audit service to the providers.
    pub fn bind_audit(&self, service: Arc<AuditService>) {
        let _ = self.shared.late.audit.set(service);
    }

    /// Hands the sync host's operations to the providers.
    pub fn bind_sync(&self, service: Arc<dyn SyncService>) {
        let _ = self.shared.late.sync.set(service);
    }

    fn provide(&self, ctx: &mut SiteCtx<'_>) -> Result<()> {
        ctx.provide::<dyn JournalStore>(Arc::clone(&self.store))?;
        ctx.provide::<dyn ManifestKeys>(Arc::clone(&self.keys))?;
        ctx.provide::<dyn AuditWiring>(Arc::clone(&self.audit))?;
        ctx.provide::<dyn JournalOps>(Arc::clone(&self.journal_ops))?;
        ctx.provide::<dyn SyncOps>(Arc::clone(&self.sync_ops))
    }
}

/// The wiring built per site, kept across a reload (a reload must not derive the keys again).
fn built() -> &'static Mutex<std::collections::HashMap<String, Wiring>> {
    static BUILT: OnceLock<Mutex<std::collections::HashMap<String, Wiring>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

fn policy_setting(policy: PolicyName, on_unreachable: OnUnreachable) -> PolicySetting {
    match policy {
        PolicyName::Local => PolicySetting::Local,
        PolicyName::ReplicaSync => PolicySetting::ReplicaSync,
        PolicyName::RemoteSync => PolicySetting::RemoteSync {
            on_unreachable: match on_unreachable {
                OnUnreachable::Degrade => WireUnreachable::Degrade,
                OnUnreachable::Halt => WireUnreachable::Halt,
            },
        },
    }
}

/// Registers the providers when the site's journal is on.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("journal_wiring")?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = built()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&site)
            .cloned();
        if let Some(wiring) = existing {
            wiring.provide(ctx)?;
        }
        return Ok(());
    }
    let journal_settings: journal::Settings = ctx.settings_or_default("journal")?;
    let Some(resolved) = journal_settings.resolve(&site)? else {
        tracing::debug!(site = %site, "journal_wiring: the journal is off; nothing is provided");
        return Ok(());
    };
    // The journal module refuses a site without [services.secrets]; here it is only a reason to
    // provide nothing (the shipper is not running either).
    let Some(secrets) = ctx.port::<SecretsHost>() else {
        return Ok(());
    };
    let provider: Arc<dyn KeyProvider> = secrets.keyring().clone();
    let journal_keys = JournalKeys::derive(Arc::clone(&provider))
        .await
        .map_err(|e| Error(format!("services.journal_wiring: {}", e.code())))?;
    let snapshot_keyring = SnapshotKeyring::derive(&provider).await.map_err(|_| {
        Error("services.journal_wiring: the snapshot keys cannot be derived".to_owned())
    })?;
    let env = *ctx.env()?;
    let mut forbidden = vec![env.root.join("sites")];
    for app in &env.loaded.common.apps {
        forbidden.push(if app.is_absolute() {
            app.clone()
        } else {
            env.root.join(app)
        });
    }
    let runtime = Handle::try_current()
        .map_err(|_| Error("services.journal_wiring: no async runtime".to_owned()))?;
    let wiring = Wiring::new(Parts {
        site: site.clone(),
        policy: policy_setting(resolved.policy, resolved.on_unreachable),
        region: resolved.region.clone().unwrap_or_default(),
        target: resolved.target.clone(),
        forbidden,
        manifest_keys: journal_keys.verifying(),
        snapshot_keys: Arc::new(snapshot_keyring.keys()),
        exclusions: Arc::clone(env.live) as Arc<dyn CaptureExclusions>,
        catalog: Arc::clone(&env.permission.catalog),
        rto_hours: settings.rto_hours,
        runtime,
    })?;
    wiring.provide(ctx)?;
    let late = wiring.clone();
    ctx.finally(move |ctx| {
        // The journal module's finaliser ran first (1100 before 1105).
        late.bind_journal(ctx.require_port::<JournalHandle>()?);
        if let Some(audit) = ctx.port::<AuditService>() {
            late.bind_audit(audit);
        }
        if let Some(sync) = ctx.port::<dyn SyncService>() {
            late.bind_sync(sync);
        }
        Ok(())
    });
    built()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, wiring);
    Ok(())
}
