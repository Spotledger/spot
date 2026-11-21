//! The journal wiring (WP-RTB-01.T9): the providers exist only for a journaled site, the store is
//! read only, the manifest keys verify what the shipper signed and nothing else, the audit's
//! journal data comes from verified manifests, and each management operation delegates or refuses
//! with a catalogued reason. No database: the pieces that need one (`register` over a whole site)
//! are covered by the harness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use spot_cli::services::journal::{FolderStore, JournalHandle, JournalKeys, JournalStore};
use spot_cli::services::journal_wiring::{
    CaptureExclusions, ExclusionRow, FolderReader, Parts, Settings, SyncService, Wiring,
    read_catalogue,
};
use spot_cli::services::{journal, journal_access, journal_audit, journal_wiring, mgmt_exec};
use spot_document::engine::ExclusionKind;
use spot_journal_audit::{DurabilityPolicy, KeyBackupStatus};
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys};
use spot_journal_recovery::{AuditFailure, AuditHistory, FactResult};
use spot_journal_snapshot_pg::SnapshotKeyring;
use spot_journal_store::{Layout, StoreError, TenantId};
use spot_mgmt_journal::ExecKind as JKind;
use spot_mgmt_journal::args::{PolicySetting, SnapshotListArgs};
use spot_mgmt_journal::results::JournalState;
use spot_mgmt_sync::args::{
    EnrolArgs, NodeArgs, PolicyShowArgs, QuarantineDiscardArgs, QuarantineRetryArgs, ResendArgs,
    RoleChangeArgs,
};
use spot_mgmt_sync::results::{
    BootstrapStatusResult, ConditionListResult, EnrolResult, NodeListResult, NodeShowResult,
    NodeStateResult, NotifyContactResult, PolicyLintResult, PolicyShowResult,
    QuarantineDiscardResult, QuarantineListResult, QuarantineRetryResult, ResendResult,
    RoleChangeResult, RotateCertificateResult,
};
use spot_mgmt_sync::{ExecError as SErr, ExecKind as SKind, SyncOps};
use spot_secrets::{KeyProvider, MemoryKeyring};
use spot_storage_contract::{Catalog, Collation};
use spot_sync_health::HealthView;

const SITE: &str = "acme";

struct Exclusions;

impl CaptureExclusions for Exclusions {
    fn current(&self) -> Option<Vec<ExclusionRow>> {
        Some(vec![ExclusionRow {
            doctype: "stock_balance".to_owned(),
            owner: "stock".to_owned(),
            kind: ExclusionKind::Derived,
        }])
    }
}

struct NoHistory;

impl AuditHistory for NoHistory {
    fn last_verified(&self) -> FactResult<Option<spot_journal_audit::VerifiedPosition>> {
        Ok(None)
    }
    fn last_failure(&self) -> FactResult<Option<AuditFailure>> {
        Ok(None)
    }
}

struct Rig {
    wiring: Wiring,
    keys: JournalKeys,
    dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let provider: Arc<dyn KeyProvider> = Arc::new(MemoryKeyring::new("site-key").unwrap());
    let keys = JournalKeys::derive(Arc::clone(&provider)).await.unwrap();
    let snapshot_keys = SnapshotKeyring::derive(&provider).await.unwrap().keys();
    let wiring = Wiring::new(Parts {
        site: SITE.to_owned(),
        policy: PolicySetting::Local,
        region: "eu-1".to_owned(),
        target: dir.path().to_path_buf(),
        forbidden: Vec::new(),
        manifest_keys: keys.verifying(),
        snapshot_keys: Arc::new(snapshot_keys),
        exclusions: Arc::new(Exclusions),
        catalog: Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap()),
        rto_hours: 4,
        runtime: tokio::runtime::Handle::current(),
    })
    .unwrap();
    Rig { wiring, keys, dir }
}

/// Writes a signed manifest of generation 1 with the segments `1..=5` and `6..=9`.
async fn put_manifest(dir: &Path, keys: &JournalKeys) {
    let store = FolderStore::new(dir).unwrap();
    let first = SegmentSummary::new(1, 1, 5, 5, [1; 32], [0; 32]);
    let second = SegmentSummary::new(1, 6, 9, 4, [2; 32], [1; 32]);
    let manifest = Manifest::first(SITE, 1_000)
        .unwrap()
        .append_segments(&[first, second])
        .unwrap();
    let signed = keys.signer().sign(&manifest);
    let key = Layout::manifest(SITE, 1).unwrap().to_key();
    store.put_new(&key, signed.to_bytes()).await.unwrap();
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn the_order_is_after_the_journal_and_before_its_consumers() {
    assert!(journal::ORDER < journal_wiring::ORDER);
    assert!(journal_wiring::ORDER < journal_access::ORDER);
    assert!(journal_wiring::ORDER < journal_audit::ORDER);
    assert!(journal_wiring::ORDER < mgmt_exec::ORDER);
    assert!(spot_cli::services::registered_names().contains(&"journal_wiring"));
}

#[test]
fn settings_default_on_and_refuse_unknown_keys() {
    let defaults = Settings::default();
    assert!(defaults.enabled);
    assert_eq!(defaults.rto_hours, 4);
    let parsed: Settings = toml::from_str("rto_hours = 8").unwrap();
    assert_eq!(parsed.rto_hours, 8);
    assert!(toml::from_str::<Settings>("bogus = 1").is_err());
}

#[tokio::test]
async fn the_store_reads_and_never_writes() {
    let rig = rig().await;
    put_manifest(rig.dir.path(), &rig.keys).await;
    let key = Layout::manifest(SITE, 1).unwrap().to_key();
    let store = &rig.wiring.store;
    assert!(store.get(&key).await.unwrap().is_some());
    assert!(store.head(&key).await.unwrap().is_some());
    assert!(
        !store
            .list(&format!("{SITE}/manifest/"), None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let other = Layout::manifest(SITE, 2).unwrap().to_key();
    assert!(store.put_new(&other, vec![1]).await.is_err());
    assert!(matches!(
        store.put_replace(&key, vec![1], "etag").await,
        Err(StoreError::ReplaceRefused { .. })
    ));
    assert!(matches!(
        store.delete(&key).await,
        Err(StoreError::DeleteRefused { .. })
    ));
    // Nothing changed.
    assert!(store.get(&other).await.unwrap().is_none());
}

#[tokio::test]
async fn a_target_inside_a_forbidden_directory_or_missing_is_refused_on_every_use() {
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("journal");
    std::fs::create_dir_all(&inside).unwrap();
    let reader = FolderReader::new(inside, vec![dir.path().to_path_buf()]);
    let key = Layout::manifest(SITE, 1).unwrap().to_key();
    assert!(reader.get(&key).await.is_err());
    let missing = FolderReader::new(dir.path().join("nowhere"), Vec::new());
    assert!(missing.get(&key).await.is_err());
}

#[tokio::test]
async fn the_manifest_keys_verify_the_shippers_manifests_and_no_others() {
    let rig = rig().await;
    put_manifest(rig.dir.path(), &rig.keys).await;
    let keys: VerifyingKeys = rig.wiring.keys.keys().unwrap();
    let store = &rig.wiring.store;
    let (segments, forks) = read_catalogue(store.as_ref(), &keys, SITE).await.unwrap();
    assert_eq!(
        segments
            .iter()
            .map(|s| (s.generation, s.first_seq, s.last_seq))
            .collect::<Vec<_>>(),
        vec![(1, 1, 5), (1, 6, 9)]
    );
    assert!(forks.is_empty());
    // Another tenant's key refuses the same manifests.
    let stranger = JournalKeys::derive(Arc::new(MemoryKeyring::new("another").unwrap()))
        .await
        .unwrap();
    let refused = read_catalogue(store.as_ref(), &stranger.verifying(), SITE).await;
    assert_eq!(refused.unwrap_err(), "journal_wiring.manifest_unverified");
}

#[tokio::test]
async fn the_audit_opens_its_journal_data_from_the_verified_manifests() {
    let rig = rig().await;
    put_manifest(rig.dir.path(), &rig.keys).await;
    let source = rig.wiring.audit.source();
    let tenant = TenantId::new(SITE).unwrap();
    // The audit's data is opened on a blocking thread, as the job does. The opened inputs hold an
    // upcast registry that is not `Send`, so the checks run on that thread and only plain values
    // come back.
    let (segments, rto, derived, fingerprints_empty) = tokio::task::spawn_blocking(move || {
        let data = source.open(&tenant).unwrap();
        (
            data.options.segments.len(),
            data.options.rto_target_millis,
            data.derived.contains("stock_balance"),
            data.fingerprints.is_empty(),
        )
    })
    .await
    .unwrap();
    assert_eq!(segments, 2);
    assert_eq!(rto, 4 * 3_600_000);
    assert!(derived);
    assert!(fingerprints_empty);
    // Another tenant is refused before anything is read.
    let source = rig.wiring.audit.source();
    let stranger = TenantId::new("other").unwrap();
    let refused = tokio::task::spawn_blocking(move || source.open(&stranger).is_err())
        .await
        .unwrap();
    assert!(refused);
}

#[tokio::test]
async fn the_report_facts_come_from_the_settings_the_progress_and_the_engine() {
    let rig = rig().await;
    let sources = rig.wiring.audit.report_sources(Arc::new(NoHistory));
    assert_eq!(sources.tenant.as_str(), SITE);
    assert_eq!(
        sources.journal.durability_policy().unwrap(),
        Some(DurabilityPolicy::Local)
    );
    assert_eq!(
        sources.journal.journal_target().unwrap().unwrap().region,
        "eu-1"
    );
    // Before the shipper's handle is bound the progress is an unreadable fact, not a default.
    assert!(sources.journal.has_offsite_history().is_err());
    rig.wiring.bind_journal(Arc::new(JournalHandle::off()));
    assert!(!sources.journal.has_offsite_history().unwrap());
    assert_eq!(
        sources.keys.key_backup().unwrap(),
        KeyBackupStatus::Unverified
    );
    assert!(sources.keys.journal_key_present().unwrap());
    let excluded = sources.exclusions.exclusions().unwrap();
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].entity, "stock_balance");
}

#[tokio::test]
async fn journal_operations_refuse_until_the_services_are_bound() {
    let rig = rig().await;
    let ops = &rig.wiring.journal_ops;
    let error = ops.status().await.unwrap_err();
    assert_eq!(error.kind, JKind::Unavailable);
    assert_eq!(error.reason, "journal_wiring_journal_not_ready");
    let error = ops.recovery_report().await.unwrap_err();
    assert_eq!(error.reason, "journal_wiring_audit_not_running");
    let error = ops.audit_run().await.unwrap_err();
    assert_eq!(error.reason, "journal_wiring_audit_not_running");

    rig.wiring.bind_journal(Arc::new(JournalHandle::off()));
    let status = ops.status().await.unwrap();
    assert_eq!(status.state, JournalState::NoTarget);
    assert!(status.shipped.is_none());
    assert_eq!(status.policy, PolicySetting::Local);
    assert_eq!(ops.policy_get().await.unwrap().policy, PolicySetting::Local);
}

#[tokio::test]
async fn operations_without_a_hook_refuse_with_their_own_reason() {
    let rig = rig().await;
    let ops = &rig.wiring.journal_ops;
    let error = ops.snapshot_take().await.unwrap_err();
    assert_eq!(error.reason, "journal_wiring_snapshot_take_not_hosted");
    let error = ops.reship().await.unwrap_err();
    assert_eq!(error.reason, "journal_wiring_reship_not_hosted");
    let error = ops.reader_list().await.unwrap_err();
    assert_eq!(error.reason, "journal_wiring_reader_grants_not_wired");
    let error = ops
        .policy_set(PolicySetting::ReplicaSync)
        .await
        .unwrap_err();
    assert_eq!(error.kind, JKind::Conflict);
}

#[tokio::test]
async fn the_snapshot_list_and_the_verification_read_the_store() {
    let rig = rig().await;
    let ops = &rig.wiring.journal_ops;
    let listed = ops
        .snapshot_list(SnapshotListArgs { limit: 10 })
        .await
        .unwrap();
    assert!(listed.snapshots.is_empty());
    put_manifest(rig.dir.path(), &rig.keys).await;
    // The manifest lists segments the store does not hold: the verification says so.
    let verified = ops.verify().await.unwrap();
    assert!(!verified.ok);
    assert!(verified.first_failure.is_some());
    assert!(verified.reason.is_some());
}

/// A sync service whose every operation fails with a reason naming it, to see the delegation.
struct FakeSync;

fn fail<T>(op: &str) -> Result<T, SErr> {
    Err(SErr::new(SKind::Failed, op))
}

#[async_trait]
#[rustfmt::skip]
impl SyncOps for FakeSync {
    async fn health(&self) -> Result<HealthView, SErr> { fail("health") }
    async fn node_list(&self) -> Result<NodeListResult, SErr> { fail("node_list") }
    async fn node_show(&self, _a: NodeArgs) -> Result<NodeShowResult, SErr> { fail("node_show") }
    async fn node_enrol(&self, _a: EnrolArgs) -> Result<EnrolResult, SErr> { fail("node_enrol") }
    async fn node_suspend(&self, _a: NodeArgs) -> Result<NodeStateResult, SErr> { fail("node_suspend") }
    async fn node_resume(&self, _a: NodeArgs) -> Result<NodeStateResult, SErr> { fail("node_resume") }
    async fn node_revoke(&self, _a: NodeArgs) -> Result<NodeStateResult, SErr> { fail("node_revoke") }
    async fn node_rotate_certificate(&self, _a: NodeArgs) -> Result<RotateCertificateResult, SErr> { fail("node_rotate_certificate") }
    async fn node_resend(&self, _a: ResendArgs) -> Result<ResendResult, SErr> { fail("node_resend") }
    async fn node_notify_contact(&self, _a: NodeArgs) -> Result<NotifyContactResult, SErr> { fail("node_notify_contact") }
    async fn condition_list(&self) -> Result<ConditionListResult, SErr> { fail("condition_list") }
    async fn quarantine_list(&self) -> Result<QuarantineListResult, SErr> { fail("quarantine_list") }
    async fn quarantine_retry(&self, _a: QuarantineRetryArgs) -> Result<QuarantineRetryResult, SErr> { fail("quarantine_retry") }
    async fn quarantine_discard(&self, _a: QuarantineDiscardArgs) -> Result<QuarantineDiscardResult, SErr> { fail("quarantine_discard") }
    async fn policy_show(&self, _a: PolicyShowArgs) -> Result<PolicyShowResult, SErr> { fail("policy_show") }
    async fn policy_lint(&self) -> Result<PolicyLintResult, SErr> { fail("policy_lint") }
    async fn role_change(&self, _a: RoleChangeArgs) -> Result<RoleChangeResult, SErr> { fail("role_change") }
    async fn bootstrap_status(&self) -> Result<BootstrapStatusResult, SErr> { fail("bootstrap_status") }
}

impl SyncService for FakeSync {}

#[tokio::test]
async fn sync_operations_refuse_without_a_sync_service_and_delegate_with_one() {
    let rig = rig().await;
    let ops = &rig.wiring.sync_ops;
    let error = ops.node_list().await.unwrap_err();
    assert_eq!(error.kind, SKind::Unavailable);
    assert_eq!(error.reason, "journal_wiring_sync_service_absent");
    let error = ops.bootstrap_status().await.unwrap_err();
    assert_eq!(error.reason, "journal_wiring_sync_service_absent");

    rig.wiring.bind_sync(Arc::new(FakeSync));
    let error = ops.node_list().await.unwrap_err();
    assert_eq!(error.reason, "node_list");
    let error = ops.condition_list().await.unwrap_err();
    assert_eq!(error.reason, "condition_list");
    let error = ops.bootstrap_status().await.unwrap_err();
    assert_eq!(error.reason, "bootstrap_status");
}
