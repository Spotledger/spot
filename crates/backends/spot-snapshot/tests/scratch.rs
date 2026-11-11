//! Dropping scratch databases and directories (WP-MIG-05.T2) against a real PostgreSQL (the
//! `SPOT_TEST_PG_URL` harness: no URL prints one `SKIPPED:` line and passes, `SPOT_REQUIRE_PG=1`
//! without one panics, so CI cannot pass by skipping). The directory tests need no server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;
use std::sync::Arc;

use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use spot_snapshot::provider::{
    Destination, RestoreOptions, RestoreTarget, SnapshotOptions, SnapshotProvider, SnapshotTarget,
};
use spot_snapshot::{
    DropOutcome, PgDumpProvider, ScratchMarker, Snapshot, drop_scratch, drop_scratch_dir,
};

struct Quiet;

impl spot_snapshot::ProgressSink for Quiet {
    fn on(&self, _event: spot_snapshot::Progress) {}
}

fn provider() -> PgDumpProvider {
    PgDumpProvider::new(
        support::pg_bin_dir(),
        Arc::new(spot_core::SystemClock::new()),
    )
}

fn database_target(config: &PgConfig, database: &str) -> RestoreTarget {
    RestoreTarget::Database {
        connection: support::credentialed_url(&config.clone().with_dbname(database)),
        database: database.to_owned(),
    }
}

/// A fresh name in the harness's sweep-safe shape.
fn scratch_name() -> String {
    format!("spot_test_{}", uuid::Uuid::now_v7().simple())
}

/// A source database with the two spot tables the take reads, one snapshot of it, and a
/// restore of that snapshot into a fresh scratch. Returns the scratch's name and its snapshot.
async fn restored_scratch(source: &TestDb, tmp: &Path) -> (String, Snapshot) {
    support::require_pg_bin_dir();
    let client = source.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE spot_meta (key text primary key, value text NOT NULL);\n\
             INSERT INTO spot_meta VALUES ('catalog_version', '1'), ('feed_schema', '1');\n\
             CREATE TABLE spot_outbox_pub (seq bigint primary key, payload text NOT NULL);\n\
             CREATE TABLE t (id bigint primary key);\nINSERT INTO t VALUES (1), (2);",
        )
        .await
        .unwrap();
    drop(client);
    let provider = provider();
    let snapshot = provider
        .take(
            &SnapshotTarget::Database {
                connection: support::credentialed_url(&source.config()),
                tenant: "tenant-a".to_owned(),
                database: source.name().to_owned(),
            },
            &Destination {
                dir: tmp.join("snap"),
            },
            &SnapshotOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .unwrap_or_else(|e| panic!("the take failed: {e}"))
        .snapshot;
    let name = scratch_name();
    provider
        .restore(
            &snapshot,
            &database_target(&source.config(), &name),
            &RestoreOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .unwrap_or_else(|e| panic!("the restore failed: {e}"));
    (name, snapshot)
}

async fn exists(source: &TestDb, name: &str) -> bool {
    source
        .admin_client()
        .await
        .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])
        .await
        .unwrap()
        .is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_then_drop_removes_the_scratch_and_a_second_drop_is_a_no_op() {
    let Some(source) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let (name, snapshot) = restored_scratch(&source, tmp.path()).await;
    assert!(exists(&source, &name).await, "the restore created {name}");

    let live = support::credentialed_url(&source.config());
    let target = database_target(&source.config(), &name);
    let marker = ScratchMarker::for_snapshot(&snapshot);
    assert_eq!(
        drop_scratch(&live, &target, &marker).await.unwrap(),
        DropOutcome::Dropped
    );
    assert!(!exists(&source, &name).await, "the scratch is gone");
    assert_eq!(
        drop_scratch(&live, &target, &marker).await.unwrap(),
        DropOutcome::NotFound,
        "dropping twice is success"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_without_the_marker_or_with_another_is_refused() {
    let Some(source) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let (marked, snapshot) = restored_scratch(&source, tmp.path()).await;
    let live = support::credentialed_url(&source.config());
    let marker = ScratchMarker::for_snapshot(&snapshot);

    // A database somebody made by hand: no marker.
    let plain = scratch_name();
    let admin = source.admin_client().await;
    admin
        .batch_execute(&format!("CREATE DATABASE \"{plain}\""))
        .await
        .unwrap();
    let error = drop_scratch(&live, &database_target(&source.config(), &plain), &marker)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "snapshot.not_scratch");
    assert!(
        exists(&source, &plain).await,
        "the unmarked database survives"
    );

    // A marked scratch, dropped with the marker of another snapshot.
    let other = ScratchMarker::for_snapshot_id("another-snapshot");
    let error = drop_scratch(&live, &database_target(&source.config(), &marked), &other)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "snapshot.not_scratch");
    assert!(
        exists(&source, &marked).await,
        "the wrongly addressed scratch survives"
    );

    admin
        .batch_execute(&format!("DROP DATABASE \"{plain}\""))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_live_database_is_never_dropped_even_with_the_marker() {
    let Some(source) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let (_scratch, snapshot) = restored_scratch(&source, tmp.path()).await;
    let marker = ScratchMarker::for_snapshot(&snapshot);
    let live = support::credentialed_url(&source.config());

    // Even a live database that someone marked is refused, and so are the system databases.
    source
        .admin_client()
        .await
        .batch_execute(&format!(
            "COMMENT ON DATABASE \"{}\" IS '{}'",
            source.name(),
            marker.as_str()
        ))
        .await
        .unwrap();
    for name in [source.name(), "postgres", "template1"] {
        let error = drop_scratch(&live, &database_target(&source.config(), name), &marker)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "snapshot.not_scratch", "{name}");
    }
    assert!(exists(&source, source.name()).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_sessions_are_terminated_before_the_drop() {
    let Some(source) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let (name, snapshot) = restored_scratch(&source, tmp.path()).await;
    let (held, connection) = tokio_postgres::connect(
        &support::credentialed_url(&source.config().with_dbname(&name)),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    held.query_one("SELECT count(*) FROM t", &[]).await.unwrap();

    let outcome = drop_scratch(
        &support::credentialed_url(&source.config()),
        &database_target(&source.config(), &name),
        &ScratchMarker::for_snapshot(&snapshot),
    )
    .await
    .unwrap();
    assert_eq!(outcome, DropOutcome::Dropped);
    assert!(
        held.query_one("SELECT 1", &[]).await.is_err(),
        "the held session was terminated"
    );
    assert!(!exists(&source, &name).await);
}

// -- directories (no server needed) ----------------------------------------------------------

fn instance_target(data_dir: &Path) -> RestoreTarget {
    RestoreTarget::Instance {
        layout: spot_pg_supervisor::layout::PgLayout {
            bin_dir: data_dir.join("bin"),
            data_dir: data_dir.to_path_buf(),
            log_dir: data_dir.join("log"),
            listen: spot_pg_supervisor::layout::Listen::Localhost,
            port: 5999,
        },
        password: String::new(),
    }
}

#[tokio::test]
async fn a_directory_under_the_root_is_removed_once() {
    let root = tempfile::TempDir::new().unwrap();
    let dir = root.path().join("rehearsal-1").join("data");
    std::fs::create_dir_all(dir.join("base")).unwrap();
    std::fs::write(dir.join("base").join("file"), b"x").unwrap();
    let target = instance_target(&dir);
    assert_eq!(
        drop_scratch_dir(root.path(), &target).await.unwrap(),
        DropOutcome::Dropped
    );
    assert!(!dir.exists());
    assert!(
        root.path().join("rehearsal-1").exists(),
        "only the data dir goes"
    );
    assert_eq!(
        drop_scratch_dir(root.path(), &target).await.unwrap(),
        DropOutcome::NotFound
    );
}

#[tokio::test]
async fn a_directory_outside_the_root_or_the_root_itself_is_refused() {
    let root = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("keep"), b"x").unwrap();
    for dir in [
        outside.path().to_path_buf(),
        root.path().to_path_buf(),
        root.path()
            .join("..")
            .join(outside.path().file_name().unwrap()),
        root.path()
            .join("a")
            .join("..")
            .join("..")
            .join("elsewhere"),
    ] {
        let error = drop_scratch_dir(root.path(), &instance_target(&dir))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "snapshot.not_scratch", "{}", dir.display());
    }
    let relative = drop_scratch_dir(root.path(), &instance_target(Path::new("data")))
        .await
        .unwrap_err();
    assert_eq!(relative.code(), "snapshot.not_scratch");
    assert!(outside.path().join("keep").exists());
    assert!(root.path().exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_below_the_root_is_not_followed() {
    let root = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(outside.path().join("data")).unwrap();
    std::fs::write(outside.path().join("data").join("keep"), b"x").unwrap();

    // The data directory itself is a link to a directory outside the root.
    let link = root.path().join("link");
    std::os::unix::fs::symlink(outside.path().join("data"), &link).unwrap();
    let error = drop_scratch_dir(root.path(), &instance_target(&link))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "snapshot.not_scratch");

    // An intermediate component is a link.
    let via = root.path().join("via");
    std::os::unix::fs::symlink(outside.path(), &via).unwrap();
    let error = drop_scratch_dir(root.path(), &instance_target(&via.join("data")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "snapshot.not_scratch");
    assert!(outside.path().join("data").join("keep").exists());
}
