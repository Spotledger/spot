//! The archive and audit adapters: a final backup that verifies its own manifest, and an audit
//! trail that links every record into the chain and fails the step when it cannot.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use spot_audit_chain::{
    AppendOutcome, AuditError, AuditStore, ChainEntry, Checkpoint, CheckpointPolicy, InMemoryStore,
    KeyRing, LocalSigner, StoreFuture, Verifier,
};
use spot_snapshot::hash::{Inventory, sha256_file};
use spot_snapshot::{
    Capabilities, Consistency, Destination, ProgressSink, RestoreOptions, RestoreTarget, Restored,
    Snapshot, SnapshotError, SnapshotManifest, SnapshotOptions, SnapshotProvider, SnapshotResult,
    SnapshotTarget, TakeOutcome, VerifyReport,
};
use spot_tenancy::ports::{Archiver, AuditEvent, AuditSink, Outcome};
use spot_tenancy::{
    ClusterId, DatabaseId, Edition, Placement, Region, Tenant, TenantFlags, TenantId, TenantName,
    TenantState,
};
use spot_tenancy_adapters::{AdapterError, ChainAuditSink, SnapshotArchiver};

fn tenant(name: &str, cluster: &str, seed: u8) -> Tenant {
    let id = TenantId::new_v7(1_700_000_000_000 + u64::from(seed), [seed; 10]);
    Tenant {
        id,
        name: TenantName::new(name).unwrap(),
        region: Region::new("eu-west").unwrap(),
        edition: Edition::new("standard").unwrap(),
        state: TenantState::Suspended,
        placement: Placement {
            cluster: ClusterId::new(cluster).unwrap(),
            database: DatabaseId::new("db1").unwrap(),
            schema: id.schema_name(),
        },
        created: 1_700_000_000_000,
        flags: TenantFlags::NONE,
        version: 1,
        retention: None,
        name_reserved_until: None,
    }
}

fn runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a test runtime"),
    )
}

// ---- archive ----------------------------------------------------------------------------

/// A provider that writes a real manifest through `spot-snapshot`'s public types, so the
/// adapter's verification runs the crate's own checksum path rather than a stub.
#[derive(Default)]
struct FakeProvider {
    takes: AtomicU32,
    fail: AtomicBool,
}

impl FakeProvider {
    fn takes(&self) -> u32 {
        self.takes.load(Ordering::SeqCst)
    }
}

impl SnapshotProvider for FakeProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: true,
            needs_stopped_instance: false,
            incremental: false,
            max_parallel: 1,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        "fake"
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        _opts: &SnapshotOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<TakeOutcome>> + Send + '_>> {
        self.takes.fetch_add(1, Ordering::SeqCst);
        let failing = self.fail.load(Ordering::SeqCst);
        let tenant = target.tenant().to_owned();
        let dir = dest.dir.clone();
        Box::pin(async move {
            if failing {
                return Err(SnapshotError::io("the injected take fails"));
            }
            tokio::fs::create_dir_all(&dir).await?;
            let data = dir.join("data.bin");
            tokio::fs::write(&data, b"tenant data, one row").await?;
            let checksum = sha256_file(&data).await?;
            let mut files = Inventory::new();
            files.insert("data.bin".to_owned(), checksum.clone());
            let manifest = SnapshotManifest {
                id: "0198c7a2-0000-7000-8000-000000000001".to_owned(),
                tenant,
                provider: "spot-snapshot".to_owned(),
                provider_version: "0.1.0".to_owned(),
                provider_kind: "fake".to_owned(),
                pg_major: spot_snapshot::SUPPORTED_MAJOR_VERSION,
                catalogue_version: "1".to_owned(),
                feed_schema: "1".to_owned(),
                outbox_generation: 0,
                outbox_last_published_seq: 0,
                started_at: "2026-01-01T00:00:00Z".to_owned(),
                finished_at: "2026-01-01T00:00:01Z".to_owned(),
                consistency: Consistency::OnlineConsistent { xmin: 1, epoch: 0 },
                total_bytes: checksum.bytes,
                file_count: 1,
                files,
                node_local: BTreeMap::new(),
                platform_extra: BTreeMap::new(),
            };
            manifest.publish(&dir).await?;
            let snapshot = Snapshot::open(&dir).await?;
            Ok(TakeOutcome { snapshot })
        })
    }

    fn restore(
        &self,
        _snapshot: &Snapshot,
        _target: &RestoreTarget,
        _opts: &RestoreOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<Restored>> + Send + '_>> {
        Box::pin(async { Err(SnapshotError::io("restore is not part of this test")) })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<VerifyReport>> + Send + '_>> {
        let snapshot = snapshot.clone();
        Box::pin(async move { snapshot.verify().await })
    }

    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<()>> + Send + '_>> {
        let dir = snapshot.dir().to_path_buf();
        Box::pin(async move {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(SnapshotError::from)
        })
    }
}

fn connections() -> Vec<((ClusterId, DatabaseId), String)> {
    vec![(
        (
            ClusterId::new("c1").unwrap(),
            DatabaseId::new("db1").unwrap(),
        ),
        "postgres://spot:secret@localhost/db1".to_owned(),
    )]
}

#[test]
fn a_final_backup_is_taken_once_replayed_and_verified() {
    let root = tempfile::tempdir().unwrap();
    let archiver = SnapshotArchiver::new(
        FakeProvider::default(),
        connections(),
        root.path(),
        runtime(),
    );
    let t = tenant("acme", "c1", 1);
    let first = archiver.final_backup(&t).unwrap();
    assert_eq!(first, root.path().join(t.id.to_string()).to_string_lossy());
    // The resumable workflow replays the step: the completed backup is found, not retaken.
    let again = archiver.final_backup(&t).unwrap();
    assert_eq!(again, first);
    assert_eq!(archiver.provider().takes(), 1);
    assert!(archiver.verify(&t, &first).unwrap());
}

#[test]
fn a_backup_whose_bytes_changed_does_not_verify() {
    let root = tempfile::tempdir().unwrap();
    let archiver = SnapshotArchiver::new(
        FakeProvider::default(),
        connections(),
        root.path(),
        runtime(),
    );
    let t = tenant("acme", "c1", 1);
    let reference = archiver.final_backup(&t).unwrap();
    let data = root.path().join(t.id.to_string()).join("data.bin");
    let mut bytes = std::fs::read(&data).unwrap();
    bytes.push(b'!');
    std::fs::write(&data, bytes).unwrap();
    assert!(
        !archiver.verify(&t, &reference).unwrap(),
        "a changed byte must fail the manifest's checksum"
    );
}

#[test]
fn only_the_tenants_own_completed_backup_verifies() {
    let root = tempfile::tempdir().unwrap();
    let archiver = SnapshotArchiver::new(
        FakeProvider::default(),
        connections(),
        root.path(),
        runtime(),
    );
    let t = tenant("acme", "c1", 1);
    let reference = archiver.final_backup(&t).unwrap();
    assert!(archiver.verify(&t, &reference).unwrap());

    // Another tenant's directory is not this tenant's archive.
    let other = tenant("beta", "c1", 2);
    assert!(!archiver.verify(&other, &reference).unwrap());
    // A directory that merely exists, with no manifest, does not verify.
    let empty = root.path().join(other.id.to_string());
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        !archiver
            .verify(&other, empty.to_string_lossy().as_ref())
            .unwrap()
    );
    // A path outside the archive root is never verifyable.
    assert!(!archiver.verify(&t, "/tmp/somewhere-else").unwrap());
}

#[test]
fn an_unconfigured_placement_or_a_failing_take_fails_the_step() {
    let root = tempfile::tempdir().unwrap();
    let archiver = SnapshotArchiver::new(
        FakeProvider::default(),
        connections(),
        root.path(),
        runtime(),
    );
    let unknown = tenant("acme", "c2", 1);
    let failure = archiver.final_backup(&unknown).unwrap_err();
    assert_eq!(failure.reason, AdapterError::NotConfigured.code());
    assert!(!failure.retryable);

    let provider = FakeProvider::default();
    provider.fail.store(true, Ordering::SeqCst);
    let archiver = SnapshotArchiver::new(provider, connections(), root.path(), runtime());
    let failure = archiver.final_backup(&tenant("beta", "c1", 2)).unwrap_err();
    assert_eq!(failure.reason, AdapterError::Backup.code());
    assert!(failure.retryable);
}

// ---- audit ------------------------------------------------------------------------------

fn event(tenant: &Tenant, action: &'static str, actor: &str) -> AuditEvent {
    AuditEvent {
        tenant: tenant.id,
        action,
        step: None,
        actor: actor.to_owned(),
        outcome: Outcome::Success,
        at: 1_700_000_000_123,
    }
}

#[test]
fn every_attempt_is_linked_into_the_chain_and_verifies() {
    let rt = runtime();
    let signer = Arc::new(LocalSigner::from_seed([3u8; 32]));
    let mut ring = KeyRing::new();
    ring.add(&signer.public_key()).unwrap();
    let sink = ChainAuditSink::new(
        InMemoryStore::new(),
        "site-1",
        signer,
        CheckpointPolicy::default(),
        Arc::clone(&rt),
    );
    let t = tenant("acme", "c1", 1);
    sink.record(&event(&t, "delete_begin", "ops")).unwrap();
    let mut failed = event(&t, "provision_step", "agent:glm");
    failed.step = Some("create_schema");
    failed.outcome = Outcome::Failure(spot_tenancy::codes::STEP_FAILED);
    sink.record(&failed).unwrap();

    let chain = sink.chain();
    let second = rt
        .block_on(chain.head())
        .unwrap()
        .expect("two records are in the chain");
    assert_eq!(second.seq, 2);
    assert_eq!(second.record.kind, "tenancy.provision_step");
    assert_eq!(
        second.record.actor, "agent:glm",
        "a qualified actor is kept"
    );
    assert_eq!(
        second
            .record
            .subject
            .as_ref()
            .map(|s| (s.doctype.as_str(), s.id.as_str())),
        Some(("tenant", t.id.to_string().as_str()))
    );
    assert_eq!(
        second.record.payload.get("outcome"),
        Some(&spot_audit_chain::PayloadValue::Text(
            spot_tenancy::codes::STEP_FAILED.to_owned()
        ))
    );
    assert_eq!(
        second.record.payload.get("step"),
        Some(&spot_audit_chain::PayloadValue::Text(
            "create_schema".to_owned()
        ))
    );
    assert_eq!(second.record.at, 1_700_000_000_123_000);
    assert!(second.record.masked);

    let report = rt
        .block_on(Verifier::new(&ring).verify_range(chain.store(), "site-1", 1, u64::MAX))
        .unwrap();
    assert!(report.is_ok(), "{report:?}");
}

#[test]
fn a_plain_control_plane_actor_becomes_a_user_and_an_absent_one_the_system() {
    let rt = runtime();
    let sink = ChainAuditSink::new(
        InMemoryStore::new(),
        "site-1",
        Arc::new(LocalSigner::from_seed([4u8; 32])),
        CheckpointPolicy::default(),
        Arc::clone(&rt),
    );
    let t = tenant("acme", "c1", 1);
    sink.record(&event(&t, "suspend", "ops")).unwrap();
    sink.record(&event(&t, "reactivate", "  ")).unwrap();
    let entries = rt.block_on(sink.chain().read_range(1, 2)).unwrap();
    assert_eq!(entries[0].record.actor, "user:ops");
    assert_eq!(entries[1].record.actor, "system");
}

#[test]
fn replaying_a_step_appends_another_record() {
    let rt = runtime();
    let sink = ChainAuditSink::new(
        InMemoryStore::new(),
        "site-1",
        Arc::new(LocalSigner::from_seed([5u8; 32])),
        CheckpointPolicy::default(),
        Arc::clone(&rt),
    );
    let t = tenant("acme", "c1", 1);
    sink.record(&event(&t, "provision_step", "ops")).unwrap();
    sink.record(&event(&t, "provision_step", "ops")).unwrap();
    assert_eq!(
        rt.block_on(sink.chain().read_range(1, u64::MAX))
            .unwrap()
            .len(),
        2,
        "the trail logs attempts; a replay adds one"
    );
}

/// A store that never accepts an append, for the failure path.
struct ClosedStore;

fn closed<T>() -> StoreFuture<'static, T> {
    Box::pin(async { Err(AuditError::Store("the store is closed".to_owned())) })
}

impl AuditStore for ClosedStore {
    fn head<'a>(&'a self, _site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        closed()
    }

    fn append_if_head<'a>(
        &'a self,
        _site: &'a str,
        _expected_seq: u64,
        _entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        closed()
    }

    fn range<'a>(
        &'a self,
        _site: &'a str,
        _from: u64,
        _to: u64,
    ) -> StoreFuture<'a, Vec<ChainEntry>> {
        closed()
    }

    fn put_checkpoint<'a>(&'a self, _checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        closed()
    }

    fn latest_checkpoint<'a>(&'a self, _site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        closed()
    }
}

#[test]
fn an_append_that_fails_fails_the_step() {
    let sink = ChainAuditSink::new(
        ClosedStore,
        "site-1",
        Arc::new(LocalSigner::from_seed([6u8; 32])),
        CheckpointPolicy::default(),
        runtime(),
    );
    let failure = sink
        .record(&event(&tenant("acme", "c1", 1), "delete_begin", "ops"))
        .unwrap_err();
    assert_eq!(failure.reason, AdapterError::Audit.code());
    assert!(failure.retryable);
}
