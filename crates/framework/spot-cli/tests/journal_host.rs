//! The journal service hosted directly (no `SiteCtx`, the same plain `Service` trait the registry
//! drives) over a real PostgreSQL outbox and a folder store: documents' events are in the journal
//! after a seal, a killed shipper leaves no gap and no duplicate position, and a folder that
//! cannot be written degrades the site, raises `RPO_BREACH` after the grace and recovers.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`; with
//! `SPOT_REQUIRE_PG=1` a missing server fails.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::Service;
use spot_cli::services::journal::{
    JournalHandle, OnUnreachable, Parts, PolicyName, Resolved, StartupCheck, build_service,
};
use spot_core::{Clock, SystemClock};
use spot_events_contract::SourceId;
use spot_events_contract::envelope::Position;
use spot_journal_manifest::VerifyingKeys;
use spot_journal_relay::{Durability, RelayConfig};
use spot_journal_segment::SealLimits;
use spot_journal_ship::memory::{MemoryLease, RecordingAlarms};
use spot_journal_ship::pg::{PgOutbox, PgWatermarks};
use spot_journal_ship::{
    AlarmKind, ChainChecker, ChainState, JournalKeys, PassThrough, PortFuture, Ports, ShipFault,
    StoreChecker, StoreRelayFactory, TenantScoped,
};
use spot_journal_store::{FolderStore, JournalStore};
use spot_journal_verify::{Mode, PinnedExpected, verify_pinned};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_secrets::MemoryKeyring;
use spot_storage_contract::{
    Catalog, Collation, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend,
    Transaction, TxOptions,
};
use tokio::sync::watch;

const SITE: &str = "acme";

struct Env {
    _db: TestDb,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    // `spot migrate` creates the shipper's table from `schema::extra_tables`; a test database
    // gets it the way the store crates' own tests do.
    let mut options = TxOptions::new(name("journal_host_ddl"), Isolation::ReadCommitted);
    options.max_writes = 100;
    let mut tx = backend.begin(Arc::clone(&catalog), options).await.unwrap();
    for statement in spot_journal_ship::pg::DDL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        tx.raw_execute_with(PgTransaction::READ, statement, &[])
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    Some(Env {
        _db: db,
        backend,
        catalog,
    })
}

/// Commits `first..=last` outbox events, one transaction each (a registered audit topic).
async fn write_events(env: &Env, first: u64, last: u64) {
    for n in first..=last {
        let payload = serde_json_payload(n);
        let mut options = TxOptions::new(name("journal_host_write"), Isolation::ReadCommitted);
        options.max_writes = 10;
        let mut tx = env
            .backend
            .begin(Arc::clone(&env.catalog), options)
            .await
            .unwrap();
        tx.enqueue(&OutboxEvent {
            id: RecordId::new_v7(),
            topic: name("sys_condition_raised"),
            subject: NodeRef {
                entity: name("condition"),
                id: RecordId::new_v7(),
            },
            subject_version: None,
            schema_version: 1,
            payload,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

fn serde_json_payload(n: u64) -> String {
    format!(
        "{{\"meta\":{{\"occurred_at\":\"2026-01-01T00:00:00Z\",\"actor\":{{\"kind\":\"system\",\
         \"component\":\"scheduler\"}},\"correlation_id\":\"01900000-0000-7000-8000-{n:012x}\"}},\
         \"data\":{{\"n\":{n}}}}}"
    )
}

struct Pass;

impl StartupCheck for Pass {
    fn check(&self) -> PortFuture<'_, Result<(), String>> {
        Box::pin(async { Result::Ok(()) })
    }
}

struct Rig {
    handle: Arc<JournalHandle>,
    service: Arc<dyn Service>,
    alarms: Arc<RecordingAlarms>,
    keys_verifying: VerifyingKeys,
}

async fn rig(env: &Env, dir: &Path, keyring: &Arc<MemoryKeyring>, batch: usize) -> Rig {
    let keys = Arc::new(JournalKeys::derive(Arc::clone(keyring) as _).await.unwrap());
    let path: PathBuf = dir.to_path_buf();
    let make_store = {
        let path = path.clone();
        Arc::new(move || {
            let folder = FolderStore::new(&path).map_err(|_| ShipFault::StoreDown {
                code: "journal_store.io",
            })?;
            TenantScoped::new(folder, SITE).map_err(|_| ShipFault::Config("tenant"))
        })
    };
    let signer_keys = Arc::clone(&keys);
    let mut relay_config =
        RelayConfig::new(SITE, SITE, Durability::RemoteSync).with_tail_keys(keys.tail_keys());
    // Seal after every batch so the segments are there to read.
    relay_config.limits = SealLimits {
        max_bytes: 1,
        max_age_millis: 0,
    };
    let factory = StoreRelayFactory::new(
        make_store,
        Arc::new(move || signer_keys.signer()),
        keys.verifying(),
        relay_config,
    );
    let checker = StoreChecker::new(
        TenantScoped::new(FolderStore::new(&path).unwrap(), SITE).unwrap(),
        keys.verifying(),
        SITE,
        SITE,
    )
    .unwrap();
    let alarms = Arc::new(RecordingAlarms::new());
    let resolved = Resolved {
        policy: PolicyName::RemoteSync,
        on_unreachable: OnUnreachable::Degrade,
        target: path,
        region: None,
        limits: SealLimits::default(),
        batch_max_entries: batch,
        interval: Duration::from_millis(20),
        rpo_grace: Duration::from_secs(1),
        verify_every: Duration::from_secs(1),
        lease: Duration::from_secs(30),
    };
    let ports = Ports {
        outbox: Arc::new(PgOutbox::new(
            env.backend.clone(),
            SourceId::new(SITE).unwrap(),
        )),
        marks: Arc::new(PgWatermarks::new(env.backend.pool())),
        lease: Arc::new(MemoryLease::held()),
        protector: Arc::new(PassThrough),
        alarms: alarms.clone(),
        factory,
        checker: Some(Arc::new(checker) as Arc<dyn ChainChecker>),
        backup: None,
    };
    let (service, handle) = build_service(Parts {
        site: SITE.to_owned(),
        resolved,
        ports,
        startup: Arc::new(Pass),
        clock: Arc::new(SystemClock::new()) as Arc<dyn Clock>,
    })
    .unwrap();
    Rig {
        handle,
        service,
        alarms,
        keys_verifying: keys.verifying(),
    }
}

fn start(rig: &Rig) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let service = Arc::clone(&rig.service);
    let task = tokio::spawn(async move {
        let _ = service.run(stop_rx).await;
    });
    (stop_tx, task)
}

/// `(first, last)` of every sealed segment in the journal, in key order.
async fn segment_ranges(dir: &Path) -> Vec<(u64, u64)> {
    let store = FolderStore::new(dir).unwrap();
    let listed = store
        .list(&format!("{SITE}/segments/"), None, 10_000)
        .await
        .unwrap();
    let mut ranges: Vec<(u64, u64)> = listed
        .iter()
        .filter_map(|entry| {
            let file = entry.key.rsplit('/').next()?;
            let stem = file.strip_suffix(".seg")?;
            let (first, last) = stem.split_once('-')?;
            Some((first.parse().ok()?, last.parse().ok()?))
        })
        .collect();
    ranges.sort_unstable();
    ranges
}

fn assert_contiguous(ranges: &[(u64, u64)], last: u64) {
    let mut next = 1;
    for (first, to) in ranges {
        assert_eq!(
            *first, next,
            "a gap or a duplicate before {first}: {ranges:?}"
        );
        next = to + 1;
    }
    assert!(
        next > last,
        "the journal stops at {} of {last}: {ranges:?}",
        next - 1
    );
}

async fn deep_clean(dir: &Path, keys: &VerifyingKeys) {
    let store = TenantScoped::new(FolderStore::new(dir).unwrap(), SITE).unwrap();
    let expected =
        PinnedExpected::new(spot_journal_store::TenantId::new(SITE).unwrap(), SITE, 1).unwrap();
    let report = verify_pinned(&store, keys, &expected, Mode::Deep)
        .await
        .unwrap();
    assert!(report.is_clean(), "{:?}", report.findings);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_are_in_the_journal_after_a_seal_and_the_chain_verifies() {
    let Some(env) = env().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let keyring = Arc::new(MemoryKeyring::new("journal-host-test").unwrap());
    write_events(&env, 1, 30).await;
    let rig = rig(&env, dir.path(), &keyring, 10).await;
    rig.service.check().await.unwrap();
    let (stop, task) = start(&rig);
    let target = Position {
        generation: 1,
        seq: 30,
    };
    assert!(
        rig.handle
            .progress()
            .wait_durable(target, Duration::from_secs(30))
            .await,
        "{}",
        rig.handle.readiness_text()
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_contiguous(&segment_ranges(dir.path()).await, 30);
    deep_clean(dir.path(), &rig.keys_verifying).await;
    assert!(
        rig.handle
            .readiness_text()
            .starts_with("journal: remote-sync")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_shipper_restarts_without_a_gap_or_a_duplicate_position() {
    let Some(env) = env().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let keyring = Arc::new(MemoryKeyring::new("journal-host-test").unwrap());
    write_events(&env, 1, 40).await;
    let first = rig(&env, dir.path(), &keyring, 4).await;
    first.service.check().await.unwrap();
    let (_stop, task) = start(&first);
    // Let it ship a few batches, then kill it mid-flight: no shutdown, no flush.
    assert!(
        first
            .handle
            .progress()
            .wait_durable(
                Position {
                    generation: 1,
                    seq: 8
                },
                Duration::from_secs(30)
            )
            .await
    );
    task.abort();
    let _ = task.await;
    write_events(&env, 41, 60).await;
    let second = rig(&env, dir.path(), &keyring, 4).await;
    second.service.check().await.unwrap();
    let (stop, task) = start(&second);
    assert!(
        second
            .handle
            .progress()
            .wait_durable(
                Position {
                    generation: 1,
                    seq: 60
                },
                Duration::from_secs(60)
            )
            .await,
        "{}",
        second.handle.readiness_text()
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_contiguous(&segment_ranges(dir.path()).await, 60);
    deep_clean(dir.path(), &second.keys_verifying).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unwritable_folder_degrades_raises_rpo_breach_after_the_grace_and_recovers() {
    use std::os::unix::fs::PermissionsExt;

    let Some(env) = env().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let keyring = Arc::new(MemoryKeyring::new("journal-host-test").unwrap());
    write_events(&env, 1, 5).await;
    let rig = rig(&env, dir.path(), &keyring, 10).await;
    rig.service.check().await.unwrap();
    let (stop, task) = start(&rig);
    assert!(
        rig.handle
            .progress()
            .wait_durable(
                Position {
                    generation: 1,
                    seq: 5
                },
                Duration::from_secs(30)
            )
            .await
    );
    // Staging a new object writes under the store's own staging directory: make that read-only.
    let tenant_dir = dir.path().join(spot_journal_store::STAGING_DIR);
    std::fs::set_permissions(&tenant_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // A process running as root writes anyway: nothing to prove then.
    if std::fs::write(tenant_dir.join("probe"), b"x").is_ok() {
        println!("SKIPPED: the process can write to a read-only folder (running as root)");
        let _ = std::fs::remove_file(tenant_dir.join("probe"));
        std::fs::set_permissions(&tenant_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        stop.send(true).unwrap();
        task.await.unwrap();
        return;
    }
    write_events(&env, 6, 12).await;
    // The site keeps trading (the task is still running) and the breach is raised after the grace.
    let mut raised = false;
    for _ in 0..200 {
        if rig.alarms.is_open(AlarmKind::RpoBreach) {
            raised = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(raised, "{}", rig.handle.readiness_text());
    assert!(rig.handle.readiness_text().contains("degraded"));
    std::fs::set_permissions(&tenant_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        rig.handle
            .progress()
            .wait_durable(
                Position {
                    generation: 1,
                    seq: 12
                },
                Duration::from_secs(60)
            )
            .await
    );
    assert!(
        !rig.alarms.is_open(AlarmKind::RpoBreach),
        "cleared on recovery"
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_contiguous(&segment_ranges(dir.path()).await, 12);
    // A chain checker over the same folder agrees.
    let checker = StoreChecker::new(
        TenantScoped::new(FolderStore::new(dir.path()).unwrap(), SITE).unwrap(),
        rig.keys_verifying.clone(),
        SITE,
        SITE,
    )
    .unwrap();
    assert_eq!(checker.check(1).await.unwrap(), ChainState::Clean);
}
