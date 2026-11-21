//! The measurement bench (the brief's item 7). `#[ignore]`d, so the normal `cargo
//! test` never pays for it: run it with
//! `cargo test -p spot-snapshot --test bench -- --ignored --nocapture`.
//!
//! It builds a 100-branch fixture (default; `SPOT_BENCH_ROWS_PER_BRANCH` scales the
//! rows per branch, default 100), takes a snapshot and restores it with each provider
//! it can reach, and reports take time, restore time, snapshot size and throughput as
//! an integer table (bytes and milliseconds only — no floats):
//!
//! - the stopped-instance provider over the fake PostgreSQL bundle runs on every
//!   machine (the same harness as the stopped tests);
//! - the pg-dump provider runs only when `SPOT_TEST_PG_URL` is set (and its client
//!   tools are discoverable — `SPOT_TEST_PG_BIN` or a standard layout), against the
//!   same 100-branch shape built as branch tables in one scratch database.
//!
//! The takes measured here are quiet takes (no concurrent writers; the contention
//! behaviour itself is the writers test's domain).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_pg_supervisor::layout::{Listen, PgLayout};
use spot_pg_supervisor::settings::PgSettings;
use spot_pg_supervisor::supervisor::{Supervisor, SupervisorState};
use spot_postgres::testing::TestDb;
use spot_snapshot::provider::{
    Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget, SnapshotOptions,
    SnapshotProvider, SnapshotTarget,
};
use spot_snapshot::{PgDumpProvider, StoppedInstanceProvider};

use support::{free_port, stage_fake_binaries, supervisor_config, wait_port};

const BRANCHES: usize = 100;
const DEFAULT_ROWS_PER_BRANCH: u64 = 100;
const READY: Duration = Duration::from_secs(10);

/// One provider's measured row.
struct Row {
    provider: &'static str,
    take_ms: u64,
    restore_ms: u64,
    snapshot_bytes: u64,
}

impl Row {
    /// Integer throughput: bytes per millisecond (`u64` division, 0 ms clamped to 1).
    fn throughput(&self) -> u64 {
        self.snapshot_bytes / self.take_ms.max(1)
    }
}

fn rows_per_branch() -> u64 {
    std::env::var("SPOT_BENCH_ROWS_PER_BRANCH")
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|rows| *rows > 0)
        .unwrap_or(DEFAULT_ROWS_PER_BRANCH)
}

fn print_table(rows: &[Row]) {
    println!();
    println!(
        "{:<22} {:>10} {:>12} {:>15} {:>22}",
        "provider", "take_ms", "restore_ms", "snapshot_bytes", "throughput_b_per_ms"
    );
    for row in rows {
        println!(
            "{:<22} {:>10} {:>12} {:>15} {:>22}",
            row.provider,
            row.take_ms,
            row.restore_ms,
            row.snapshot_bytes,
            row.throughput()
        );
    }
    println!(
        "fixture: {BRANCHES} branches, {} rows per branch; quiet takes; wall-clock ms",
        rows_per_branch()
    );
}

/// One progress sink that keeps the crate's no-printing promise in the bench too.
struct Quiet;

impl ProgressSink for Quiet {
    fn on(&self, _event: Progress) {}
}

/// A port probe.
async fn port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// Starts and stops the fake once, leaving the stopped instance behind (the caller
/// contract `take_stopped` builds on).
async fn start_and_stop_once(supervisor: &Supervisor, port: u16) {
    let handle = supervisor.start().await.unwrap();
    assert!(wait_port(port, READY).await, "the fake never listened");
    handle.stop(Duration::from_secs(2)).await.unwrap();
    assert!(!port_open(port).await, "the port was freed");
}

/// One staged fake environment: a bin dir with the three roles, an initialised data
/// dir whose branch markers carry the bench's row count, and a free port in `.env`.
struct FakeEnv {
    _tmp: tempfile::TempDir,
    layout: PgLayout,
    supervisor: Supervisor,
}

impl FakeEnv {
    fn new(branches: usize, rows_per_branch: usize) -> FakeEnv {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let data_dir = tmp.path().join("data");
        let log_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&bin_dir).unwrap();
        stage_fake_binaries(&bin_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        for (name, contents) in [
            ("PG_VERSION", "18\n"),
            ("spot.conf", "port = 5433\n"),
            ("postgresql.conf", "# fake\n"),
        ] {
            std::fs::write(data_dir.join(name), contents).unwrap();
        }
        for branch in 0..branches {
            let dir = data_dir.join(format!("branch-{branch:03}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut content = String::new();
            for _ in 0..rows_per_branch {
                content.push_str("0123456789abcdefghij0123456789abcdefghij\n");
            }
            std::fs::write(dir.join("rows.bin"), content).unwrap();
        }
        let port = free_port();
        std::fs::write(data_dir.join(".env"), format!("FAKE_PORT={port}\n")).unwrap();
        let layout = PgLayout {
            bin_dir,
            data_dir,
            log_dir,
            listen: Listen::Localhost,
            port,
        };
        let supervisor =
            Supervisor::new(layout.clone(), PgSettings::default(), supervisor_config());
        FakeEnv {
            _tmp: tmp,
            layout,
            supervisor,
        }
    }
}

fn instance_target(layout: &PgLayout, tenant: &str) -> SnapshotTarget {
    SnapshotTarget::Instance {
        layout: layout.clone(),
        tenant: tenant.to_owned(),
        password: "not-used-by-the-bench".to_owned(),
    }
}

/// The bench itself: one row per reachable provider, then the integer table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_providers_100_branches() {
    let mut rows = Vec::new();
    rows.push(bench_stopped().await);
    if std::env::var("SPOT_TEST_PG_URL").is_ok_and(|url| !url.is_empty()) {
        match bench_pgdump().await {
            Some(row) => rows.push(row),
            None => println!("the pg_dump row was skipped (see the SKIPPED line above)"),
        }
    } else {
        println!("SKIPPED: set SPOT_TEST_PG_URL for the pg_dump provider's row");
    }
    print_table(&rows);
}

/// The stopped-instance bench row over the fake bundle. Restore replaces a second
/// stopped instance's data dir; the taken snapshot's verify is the correctness gate.
async fn bench_stopped() -> Row {
    let rows_per_branch = usize::try_from(rows_per_branch()).unwrap_or(usize::MAX);
    let source = FakeEnv::new(BRANCHES, rows_per_branch);
    start_and_stop_once(&source.supervisor, source.layout.port).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let provider = StoppedInstanceProvider::new(Arc::new(spot_core::SystemClock::new()));
    let target = instance_target(&source.layout, "bench-tenant");
    let started = Instant::now();
    let take = provider
        .take(&target, &dest, &SnapshotOptions::default(), Arc::new(Quiet))
        .await
        .unwrap();
    let take_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    assert!(
        take.snapshot.verify().await.is_ok(),
        "the taken snapshot verifies"
    );

    // A second stopped instance as the replace target (the round trip's shape).
    let target_env = FakeEnv::new(1, 1);
    start_and_stop_once(&target_env.supervisor, target_env.layout.port).await;
    let token = target_env.layout.data_dir.display().to_string();
    let restore_started = Instant::now();
    let restored = provider
        .restore(
            &take.snapshot,
            &RestoreTarget::Instance {
                layout: target_env.layout.clone(),
                password: "not-used-by-the-bench".to_owned(),
            },
            &RestoreOptions {
                parallel: 4,
                replace: true,
                replace_token: token,
                deadline: Duration::ZERO,
            },
            Arc::new(Quiet),
        )
        .await
        .unwrap();
    let restore_ms = u64::try_from(restore_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    restored
        .snapshot
        .verify()
        .await
        .expect("the restored bytes verify");

    Row {
        provider: "stopped (fake bundle)",
        take_ms,
        restore_ms,
        snapshot_bytes: take.snapshot.manifest().total_bytes,
    }
}

/// The pg-dump bench row against the real server: 100 branch tables built in one
/// scratch database, one exported snapshot, restore into a fresh database that is
/// dropped afterwards.
async fn bench_pgdump() -> Option<Row> {
    let Some(source_db) = TestDb::create().await else {
        return None; // the harness printed the SKIPPED line (or panicked under REQUIRE)
    };
    let rows_per_branch = usize::try_from(rows_per_branch()).unwrap_or(usize::MAX);
    build_pg_fixture(&source_db, rows_per_branch).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let provider = PgDumpProvider::new(
        support::pg_bin_dir(),
        Arc::new(spot_core::SystemClock::new()),
    );
    let target = SnapshotTarget::Database {
        connection: support::credentialed_url(&source_db.config()),
        tenant: "bench-tenant".to_owned(),
        database: source_db.name().to_owned(),
    };
    let started = Instant::now();
    let take = provider
        .take(&target, &dest, &SnapshotOptions::default(), Arc::new(Quiet))
        .await
        .unwrap();
    let take_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    assert!(
        take.snapshot.verify().await.is_ok(),
        "the taken snapshot verifies"
    );

    let restore_name = format!("spot_test_{}", uuid::Uuid::now_v7().simple());
    let restore_started = Instant::now();
    let restored = provider
        .restore(
            &take.snapshot,
            &RestoreTarget::Database {
                connection: support::credentialed_url(
                    &source_db.config().with_dbname(&restore_name),
                ),
                database: restore_name.clone(),
            },
            &RestoreOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .unwrap();
    let restore_ms = u64::try_from(restore_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    assert!(restored.activated, "a database restore is live at once");
    drop(restored);

    let row = Row {
        provider: "pg_dump (real server)",
        take_ms,
        restore_ms,
        snapshot_bytes: take.snapshot.manifest().total_bytes,
    };

    // Drop the scratch restore database (best effort; the sweep is the backstop).
    if let Ok((client, conn)) = tokio_postgres::connect(
        &support::credentialed_url(&source_db.config().clone().with_dbname("postgres")),
        tokio_postgres::NoTls,
    )
    .await
    {
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let _ = client
            .execute(&format!("DROP DATABASE IF EXISTS \"{restore_name}\""), &[])
            .await;
    }
    Some(row)
}

/// The bench's fixture: the spot system tables the take's facts read plus
/// `BRANCHES` branch tables of `rows_per_branch` rows each (ids 1..=n, payloads
/// padded to real bytes).
async fn build_pg_fixture(db: &TestDb, rows_per_branch: usize) {
    let client = db.raw_client().await;
    let mut schema = String::from(
        "CREATE TABLE spot_meta (key text primary key, value text NOT NULL);\n\
         INSERT INTO spot_meta VALUES ('catalog_version', '1'), ('feed_schema', '1');\n\
         CREATE TABLE spot_outbox_pub (seq bigint primary key, payload text NOT NULL);\n",
    );
    let filler: String = "x".repeat(84);
    for branch in 0..BRANCHES {
        schema.push_str(&format!(
            "CREATE TABLE branch_{branch:03} (id bigint primary key, payload text NOT NULL);\n\
             INSERT INTO branch_{branch:03} SELECT g, '{filler}' || g \
             FROM generate_series(1, {rows_per_branch}) g;\n"
        ));
    }
    client.batch_execute(&schema).await.unwrap();
}

// Unused here but kept symmetric with the stopped tests' harness (the supervisor
// state is observed there; the bench relies on ports alone).
#[allow(dead_code)]
fn _state_probe(supervisor: &Supervisor) -> bool {
    matches!(supervisor.state(), SupervisorState::Failed { .. })
}

// Path import kept for the fixture writer's explicitness in future variants.
#[allow(dead_code)]
fn _path_of(data_dir: &Path) -> &Path {
    data_dir
}
