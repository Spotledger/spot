//! The snapshot port over `spot-snapshot`: take, resolve after a restart, restore, and the
//! timing capture. A fake provider proves the adapter; a real `pg_dump` provider runs when the
//! client tools are discoverable, so the adapter is also proven against the shipped provider.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_core::SystemClock;
use spot_migration_apply::snapshot::{SnapshotId, SnapshotPort};
use spot_migration_apply_pg::{PgApplySchema, PgSnapshotPort, RecordingTimings, TimingPhase};
use spot_snapshot::{
    Capabilities, Destination, ProgressSink, RestoreOptions, RestoreTarget, Restored, Snapshot,
    SnapshotError, SnapshotOptions, SnapshotProvider, SnapshotTarget, TakeOutcome, VerifyReport,
};
use support::*;

/// A provider that writes a manifest (and one data file) and restores by recording the call.
///
/// It is a real `SnapshotProvider`, so the adapter under test is the production one; only the
/// data movement is replaced.
struct FakeProvider {
    takes: AtomicU64,
    restores: AtomicU64,
}

impl FakeProvider {
    fn new() -> Self {
        Self {
            takes: AtomicU64::new(0),
            restores: AtomicU64::new(0),
        }
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
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TakeOutcome, SnapshotError>> + Send + '_>,
    > {
        let tenant = target.tenant().to_owned();
        let dir = dest.dir.clone();
        Box::pin(async move {
            self.takes.fetch_add(1, Ordering::SeqCst);
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(SnapshotError::from)?;
            tokio::fs::write(dir.join("data.bin"), b"tenant bytes")
                .await
                .map_err(SnapshotError::from)?;
            let id = uuid_like();
            let manifest = spot_snapshot::SnapshotManifest {
                id: id.clone(),
                tenant,
                provider: "spot-snapshot".to_owned(),
                provider_version: "0.1.0".to_owned(),
                provider_kind: "fake".to_owned(),
                pg_major: spot_snapshot::SUPPORTED_MAJOR_VERSION,
                catalogue_version: "1".to_owned(),
                feed_schema: "1".to_owned(),
                outbox_generation: 1,
                outbox_last_published_seq: 0,
                started_at: "2026-10-19T00:00:00Z".to_owned(),
                finished_at: "2026-10-19T00:00:01Z".to_owned(),
                consistency: spot_snapshot::Consistency::Offline,
                total_bytes: 12,
                file_count: 1,
                files: [(
                    "data.bin".to_owned(),
                    spot_snapshot::hash::FileChecksum {
                        sha256: sha256_hex(b"tenant bytes"),
                        bytes: 12,
                    },
                )]
                .into_iter()
                .collect(),
                node_local: Default::default(),
                platform_extra: Default::default(),
            };
            manifest.publish(&dir).await?;
            Ok(TakeOutcome {
                snapshot: Snapshot::open(&dir).await?,
            })
        })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        _target: &RestoreTarget,
        _opts: &RestoreOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Restored, SnapshotError>> + Send + '_>,
    > {
        let id = snapshot.manifest().id.clone();
        let dir = snapshot.dir().to_path_buf();
        Box::pin(async move {
            self.restores.fetch_add(1, Ordering::SeqCst);
            Ok(Restored {
                snapshot: Snapshot::open(dir).await?,
                location: format!("restored-{id}"),
                activated: true,
                replaced_aside: None,
            })
        })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<VerifyReport, SnapshotError>> + Send + '_>,
    > {
        let manifest = snapshot.manifest().clone();
        Box::pin(async move {
            Ok(VerifyReport {
                files_checked: manifest.file_count,
                bytes_checked: manifest.total_bytes,
            })
        })
    }

    fn delete(
        &self,
        _snapshot: &Snapshot,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SnapshotError>> + Send + '_>>
    {
        Box::pin(async { Ok(()) })
    }
}

fn uuid_like() -> String {
    // A time-ordered UUIDv7-shaped id: the manifest's canonical checks require version 7.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("019a0000-0000-7000-8000-{:012x}", now % 0xffff_ffff_ffff)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A fresh, empty destination root for one test. The caller keeps the guard alive to the end of
/// the test (a restarted port reuses `dir.path()`), and the directory is removed on drop.
fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("spot-migration-apply-pg-snapshot-")
        .tempdir()
        .expect("a unique temp destination")
}

fn target() -> SnapshotTarget {
    SnapshotTarget::Database {
        connection: "postgres://spot:spot@127.0.0.1:5432/postgres".to_owned(),
        tenant: "test-site".to_owned(),
        database: "postgres".to_owned(),
    }
}

fn restore_target() -> RestoreTarget {
    RestoreTarget::Database {
        connection: "postgres://spot:spot@127.0.0.1:5432/postgres".to_owned(),
        database: "postgres".to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn take_indexes_the_snapshot_so_a_restart_can_resolve_it_and_restore_records_its_time() {
    let Some(db) = db().await else { return };
    let dir = temp_dir();
    let pool = pool(&db, 4);
    let schema = PgApplySchema::new(pool.clone());
    schema.prepare().await.unwrap();
    let timings = Arc::new(RecordingTimings::new());
    let provider = Arc::new(FakeProvider::new());
    let port = PgSnapshotPort::new(
        schema,
        site(),
        provider.clone(),
        target(),
        restore_target(),
        dir.path().to_path_buf(),
        Arc::new(SystemClock::new()),
    )
    .with_timings(timings.clone());

    let id = port.take("before-apply", 1_000).await.unwrap();
    assert!(port.exists(&id).await.unwrap());
    assert!(
        !port
            .exists(&SnapshotId::new("not-a-snapshot").unwrap())
            .await
            .unwrap()
    );
    assert_eq!(
        port.snapshot_dir(&id).await.unwrap().unwrap(),
        dir.path().join("test-site-0001-before-apply")
    );

    // A fresh port over the same pool (a restarted process) resolves the id from the control
    // schema, not from memory.
    let restarted = PgSnapshotPort::new(
        PgApplySchema::new(pool),
        site(),
        provider.clone(),
        target(),
        restore_target(),
        dir.path().to_path_buf(),
        Arc::new(SystemClock::new()),
    );
    assert!(restarted.exists(&id).await.unwrap());
    let outcome = restarted.restore(&id, 2_000).await.unwrap();
    assert_eq!(outcome.snapshot, id);
    assert_eq!(outcome.at, 2_000);
    assert!(outcome.location.starts_with("restored-"));
    assert_eq!(provider.restores.load(Ordering::SeqCst), 1);

    let take_times = timings.of(TimingPhase::Snapshot);
    assert_eq!(take_times.len(), 1);
    assert_eq!(take_times[0].snapshot, id.as_str());
    // The restored timing is recorded by the port that ran the restore, not the restarted one
    // without a sink; the take's is here.
    print_timings("snapshot port", &timings.records());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restore_of_an_unknown_snapshot_is_refused() {
    let Some(db) = db().await else { return };
    let dir = temp_dir();
    let pool = pool(&db, 4);
    let schema = PgApplySchema::new(pool.clone());
    schema.prepare().await.unwrap();
    let port = PgSnapshotPort::new(
        schema,
        site(),
        Arc::new(FakeProvider::new()),
        target(),
        restore_target(),
        dir.path().to_path_buf(),
        Arc::new(SystemClock::new()),
    );
    let error = port
        .restore(&SnapshotId::new("nope").unwrap(), 1_000)
        .await
        .expect_err("an unknown snapshot");
    assert!(error.to_string().contains("snapshot_unknown"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_failure_is_reported_and_never_indexed() {
    let Some(db) = db().await else { return };
    let dir = temp_dir();
    let pool = pool(&db, 4);
    let schema = PgApplySchema::new(pool.clone());
    schema.prepare().await.unwrap();
    struct FailingProvider;
    impl SnapshotProvider for FailingProvider {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                consistent_online: false,
                needs_stopped_instance: true,
                incremental: false,
                max_parallel: 1,
                estimated_throughput: 0,
            }
        }
        fn name(&self) -> &'static str {
            "failing"
        }
        fn take(
            &self,
            _target: &SnapshotTarget,
            _dest: &Destination,
            _opts: &SnapshotOptions,
            _progress: Arc<dyn ProgressSink>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<TakeOutcome, SnapshotError>> + Send + '_>,
        > {
            Box::pin(async { Err(SnapshotError::io("the disk is full")) })
        }
        fn restore(
            &self,
            _snapshot: &Snapshot,
            _target: &RestoreTarget,
            _opts: &RestoreOptions,
            _progress: Arc<dyn ProgressSink>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Restored, SnapshotError>> + Send + '_>,
        > {
            Box::pin(async { Err(SnapshotError::io("nothing to restore")) })
        }
        fn verify(
            &self,
            _snapshot: &Snapshot,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<VerifyReport, SnapshotError>> + Send + '_>,
        > {
            Box::pin(async { Err(SnapshotError::io("nothing to verify")) })
        }
        fn delete(
            &self,
            _snapshot: &Snapshot,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), SnapshotError>> + Send + '_>,
        > {
            Box::pin(async { Ok(()) })
        }
    }
    let port = PgSnapshotPort::new(
        schema,
        site(),
        Arc::new(FailingProvider),
        target(),
        restore_target(),
        dir.path().to_path_buf(),
        Arc::new(SystemClock::new()),
    );
    let error = port
        .take("before-apply", 1_000)
        .await
        .expect_err("the take fails");
    assert!(error.to_string().contains("snapshot_failed"), "{error}");
    // Nothing was indexed.
    let client = pool.get().await.unwrap();
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM spot_migration_control.snapshot WHERE site = $1",
            &[&site().as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

/// The real `pg_dump` provider over the scratch database, when the client tools are on the
/// machine. `SPOT_TEST_PG_BIN` names their directory; otherwise `pg_dump` on `PATH` is tried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_pg_dump_snapshot_restores_with_its_timing_recorded() {
    let Some(db) = db().await else { return };
    let dir = temp_dir();
    let Some(bin) = pg_bin() else {
        println!("SKIPPED: no pg_dump client tools (set SPOT_TEST_PG_BIN)");
        return;
    };
    let config = db.config();
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        config.user, config.password, config.host, config.port, config.dbname
    );
    let pool = pool(&db, 4);
    let schema = PgApplySchema::new(pool.clone());
    schema.prepare().await.unwrap();
    let provider = Arc::new(spot_snapshot::PgDumpProvider::new(
        bin,
        Arc::new(SystemClock::new()),
    ));
    let target = SnapshotTarget::Database {
        connection: url.clone(),
        tenant: "test-site".to_owned(),
        database: config.dbname.clone(),
    };
    let restore = RestoreTarget::Database {
        connection: url,
        database: format!("{}.restore", config.dbname),
    };
    let timings = Arc::new(RecordingTimings::new());
    let port = PgSnapshotPort::new(
        schema,
        site(),
        provider,
        target,
        restore,
        dir.path().to_path_buf(),
        Arc::new(SystemClock::new()),
    )
    .with_timings(timings.clone());
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE spot_meta (key text primary key, value text NOT NULL);\n\
             INSERT INTO spot_meta VALUES ('catalog_version', '1'), ('feed_schema', '1');\n\
             CREATE TABLE spot_outbox_pub (seq bigint primary key, payload text NOT NULL);\n\
             CREATE TABLE branch_1 (id uuid PRIMARY KEY, note text);",
        )
        .await
        .unwrap();
    // The take reads the database facts from `spot_meta` and `spot_outbox_pub`; the scratch
    // database holds only what `prepare()` made, so the fixture creates them (minimal shapes,
    // as `spot-snapshot`'s own pg_dump test does).
    let id = port
        .take("before-apply", 1_000)
        .await
        .unwrap_or_else(|e| panic!("pg_dump take failed: {e:?}"));
    assert!(port.exists(&id).await.unwrap());
    let dir = port.snapshot_dir(&id).await.unwrap().expect("snapshot dir");
    let manifest = Snapshot::open(&dir).await.unwrap();
    assert_eq!(manifest.manifest().catalogue_version, "1");
    assert_eq!(manifest.manifest().feed_schema, "1");
    let snapshot = timings.of(TimingPhase::Snapshot);
    assert_eq!(snapshot.len(), 1);
    print_timings("pg_dump snapshot", &snapshot);
}

/// The directory holding the PostgreSQL client tools, when they can be run here.
///
/// `SPOT_TEST_PG_BIN` names it directly (the CI convention); otherwise the first `pg_dump` on
/// `PATH` is resolved to its directory. A machine with neither skips the test with a line, as
/// `spot-snapshot`'s own tests do.
fn pg_bin() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("SPOT_TEST_PG_BIN") {
        let path = PathBuf::from(root);
        if path.join("pg_dump").is_file() {
            return Some(path);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("pg_dump");
        if candidate.is_file() {
            let runs = std::process::Command::new(&candidate)
                .arg("--version")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false);
            if runs {
                return Some(dir);
            }
        }
    }
    None
}
