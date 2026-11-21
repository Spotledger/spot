//! Provider-level tests of the pg-dump provider against a real PostgreSQL (the
//! `SPOT_TEST_PG_URL` harness of `spot-postgres::testing`, whose skip discipline is
//! mirrored exactly: no URL prints one `SKIPPED:` line and passes; `SPOT_REQUIRE_PG=1`
//! with no URL panics — CI can never pass by skipping). The tests exercise the take of
//! a fixture schema in a single exported snapshot and the restore of it, run while
//! eight concurrent writers hammer their own tables, and prove the never-torn
//! invariant: each restored table is exactly one instant's view of that table.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use spot_snapshot::provider::{
    Destination, RestoreOptions, RestoreTarget, SnapshotOptions, SnapshotProvider, SnapshotTarget,
};
use spot_snapshot::{PgDumpProvider, Snapshot};

const WRITERS: usize = 8;
/// Rows each writer has certainly committed before the take is ever invoked: the
/// writers are polled until every counter shows this many commits, which order the
/// commits strictly before the take's export (the horizon must contain them).
const COMMITS_BEFORE_TAKE: u64 = 5;

/// The pg-dump provider for tests: the PostgreSQL 18 client tools found by the
/// support module (`SPOT_TEST_PG_BIN`, the standard install layouts, or `PATH`).
fn provider() -> PgDumpProvider {
    PgDumpProvider::new(
        support::pg_bin_dir(),
        Arc::new(spot_core::SystemClock::new()),
    )
}

fn database_target(config: PgConfig, tenant: &str, database: &str) -> SnapshotTarget {
    SnapshotTarget::Database {
        connection: support::credentialed_url(&config),
        tenant: tenant.to_owned(),
        database: database.to_owned(),
    }
}

fn restore_target(config: PgConfig, database: &str) -> RestoreTarget {
    RestoreTarget::Database {
        connection: support::credentialed_url(&config),
        database: database.to_owned(),
    }
}

/// SHA-256 hex of some bytes (the brief's table-by-table content hash).
fn sha_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The payload digest of one fixture table: every row's `id|payload` line, in id
/// order. Column layout is shared by every fixture table, source and restored alike.
async fn table_digest(client: &tokio_postgres::Client, table: &str) -> String {
    let rows = client
        .query(&format!("SELECT id, payload FROM {table} ORDER BY id"), &[])
        .await
        .unwrap_or_else(|e| panic!("reading {table} failed: {e}"));
    let mut text = String::new();
    for row in rows {
        let id: i64 = row.get(0);
        let payload: String = row.get(1);
        text.push_str(&format!("{id}|{payload}\n"));
    }
    sha_hex(text.as_bytes())
}

/// The row ids present in one fixture table, ascending.
async fn table_ids(client: &tokio_postgres::Client, table: &str) -> Vec<i64> {
    let rows = client
        .query(&format!("SELECT id FROM {table} ORDER BY id"), &[])
        .await
        .unwrap_or_else(|e| panic!("reading {table} failed: {e}"));
    rows.into_iter().map(|row| row.get::<_, i64>(0)).collect()
}

/// Builds the fixture: eight branch tables (ids 1..=8, payload mixes the branch and
/// row numbers), the spot system tables the take's fact reads touch (`spot_meta` for
/// the catalogue facts, `spot_outbox_pub` for the feed position — minimal shapes, the
/// schema contract itself is spot-postgres's domain). Returns each table's digest.
async fn create_fixture_schema(client: &tokio_postgres::Client) -> Vec<(String, String)> {
    let mut schema = String::from(
        "CREATE TABLE spot_meta (key text primary key, value text NOT NULL);\n\
         INSERT INTO spot_meta VALUES ('catalog_version', '1'), ('feed_schema', '1');\n\
         CREATE TABLE spot_outbox_pub (seq bigint primary key, payload text NOT NULL);\n",
    );
    for branch in 0..WRITERS {
        schema.push_str(&format!(
            "CREATE TABLE branch_{branch:03} (id bigint primary key, payload text NOT NULL);\n\
             INSERT INTO branch_{branch:03} SELECT g, 'branch {branch:03} row ' || g \
             FROM generate_series(1, 8) g;\n"
        ));
    }
    client.batch_execute(&schema).await.unwrap();
    let mut tables = Vec::new();
    for branch in 0..WRITERS {
        let name = format!("branch_{branch:03}");
        let digest = table_digest(client, &name).await;
        tables.push((name, digest));
    }
    tables
}

/// One progress sink that keeps the crate's no-printing promise in the tests too.
struct Quiet;

impl spot_snapshot::ProgressSink for Quiet {
    fn on(&self, _event: spot_snapshot::Progress) {}
}

/// Takes a snapshot of `target` into `dest` and returns the opened snapshot.
async fn take(
    test_provider: &PgDumpProvider,
    target: &SnapshotTarget,
    dest: &Destination,
) -> Snapshot {
    test_provider
        .take(target, dest, &SnapshotOptions::default(), Arc::new(Quiet))
        .await
        .unwrap_or_else(|e| panic!("the take failed: {e}"))
        .snapshot
}

/// Asserts the manifest's consistency statement is the online-consistent horizon of a
/// pg-dump snapshot (the exported snapshot's own xmin and epoch are what went in).
fn horizon_of(snapshot: &Snapshot) -> (u64, u64) {
    match snapshot.manifest().consistency {
        spot_snapshot::Consistency::OnlineConsistent { xmin, epoch } => (xmin, epoch),
        spot_snapshot::Consistency::Offline => {
            panic!("a pg-dump snapshot is online-consistent, the manifest says offline")
        }
    }
}

/// The round trip: a fixture schema taken in one exported snapshot, restored into a
/// fresh database, every branch table content-identical row by row, and `spot_meta`
/// (a spot system table) whole — the manifest's node-local counts agree with a live
/// read of the restored site.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgdump_round_trip_matches_table_by_table() {
    let Some(source_db) = TestDb::create().await else {
        return; // the harness printed the SKIPPED line (or panicked under REQUIRE)
    };
    support::require_pg_bin_dir();
    let client = source_db.raw_client().await;
    let digests = create_fixture_schema(&client).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let test_provider = provider();
    let target = database_target(source_db.config(), "tenant-a", source_db.name());
    let snapshot = take(&test_provider, &target, &dest).await;
    let manifest = snapshot.manifest();
    assert!(
        manifest.files.keys().any(|rel| rel.starts_with("dump/")),
        "the directory-format dump is inside the snapshot: {:?}",
        manifest.files.keys().take(12).collect::<Vec<_>>()
    );
    let (xmin, _epoch) = horizon_of(&snapshot);
    assert!(xmin > 0, "the exported horizon names its xmin");
    drop(client); // the held source connection must not linger under the restore

    // Restore into a non-existent database name on the same scratch server: the
    // provider creates the database itself (that is its contract — restore = create
    // + load, or replace after rename-aside). The name matches the TestDb harness's
    // sweep pattern (`spot_test_` + 32 hex) so a later run's sweep also cleans it if
    // the explicit drop below cannot.
    let restore_name = format!("spot_test_{}", uuid::Uuid::now_v7().simple());
    let restored = test_provider
        .restore(
            &snapshot,
            &restore_target(source_db.config().with_dbname(&restore_name), &restore_name),
            &RestoreOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .unwrap_or_else(|e| panic!("the restore failed: {e}"));
    assert_eq!(restored.location, restore_name);
    assert!(restored.activated, "a database restore is live at once");

    // A direct connection to the restored database for the content checks.
    let (restored_client, restored_conn) = tokio_postgres::connect(
        &support::credentialed_url(&source_db.config().with_dbname(&restore_name)),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap_or_else(|e| panic!("cannot connect to the restored database: {e}"));
    tokio::spawn(async move {
        let _ = restored_conn.await;
    });
    for (name, digest) in &digests {
        assert_eq!(
            &table_digest(&restored_client, name).await,
            digest,
            "table {name} differs after the restore"
        );
    }
    let meta_rows: i64 = restored_client
        .query_one("SELECT COUNT(*) FROM spot_meta", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        u64::try_from(meta_rows).unwrap(),
        2,
        "spot_meta restored whole"
    );

    // The manifest recorded the system-table counts at the take; the restored site
    // must reproduce them (the shipped checks' core, read live here).
    for (key, value) in &manifest.node_local {
        if let Some(table) = key.strip_prefix("system_counts.") {
            let live: i64 = restored_client
                .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
                .await
                .unwrap_or_else(|e| panic!("counting restored {table}: {e}"))
                .get(0);
            let recorded: u64 = value.parse().unwrap_or_else(|e| panic!("{key}: {e}"));
            assert_eq!(
                u64::try_from(live).unwrap_or(u64::MAX),
                recorded,
                "restored {table} row count differs from the manifest's record"
            );
        }
    }
    drop(restored_client);
    drop_database(&source_db.config(), &restore_name).await;
}

/// Drops a scratch test database best-effort after a restore; the TestDb sweep is
/// the backstop (the name keeps the sweep-safe `spot_test_<32-hex>` shape).
async fn drop_database(config: &PgConfig, name: &str) {
    let Ok((client, conn)) = tokio_postgres::connect(
        &support::credentialed_url(&config.clone().with_dbname("postgres")),
        tokio_postgres::NoTls,
    )
    .await
    else {
        return;
    };
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let _ = client
        .execute(&format!("DROP DATABASE IF EXISTS \"{name}\""), &[])
        .await;
}

/// The contention: every writer commits rows under way while the take holds its
/// horizon, and the restore equals the state at the exported snapshot — never torn.
///
/// What is provable, honestly, from outside a snapshot whose export instant is
/// internal to the provider: (i) per table the restored ids form the contiguous
/// prefix {1..=k} — the horizon view of sequential single-insert commits; a torn dump
/// (a worker reading later state) cannot produce a gapless prefix that also satisfies
/// (ii) and (iii); (ii) k >= commits made strictly before the take was invoked
/// (deterministic, via the pre-take barrier): the snapshot misses nothing;
/// (iii) k <= the counter as read the moment the take returned: no commit that landed
/// after the whole take (and therefore after the export) appears — no phantoms.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn pgdump_take_under_8_writers_restores_the_exported_state() {
    let Some(source_db) = TestDb::create().await else {
        return;
    };
    support::require_pg_bin_dir();
    let client = source_db.raw_client().await;
    create_fixture_schema(&client).await;
    drop(client);

    // Each of the 8 writers inserts into ITS OWN table only (no cross-table churn),
    // one id at a time starting at 9 (ids 1..=8 are the fixture's), committing at a
    // short cadence, and reports its committed count in a shared atomic.
    let committed: Vec<Arc<AtomicU64>> =
        (0..WRITERS).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let stop = Arc::new(AtomicU64::new(0));
    let url = support::credentialed_url(&source_db.config());
    let mut writers = Vec::new();
    for (writer, counter) in committed.iter().enumerate() {
        let writer = u32::try_from(writer).unwrap_or(u32::MAX);
        let url = url.clone();
        let counter = counter.clone();
        let stop = stop.clone();
        writers.push(tokio::spawn(async move {
            let (wclient, wconn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .unwrap_or_else(|e| panic!("writer {writer} cannot connect: {e}"));
            tokio::spawn(async move {
                let _ = wconn.await;
            });
            let table = format!("branch_{writer:03}");
            let mut tick: u64 = 8;
            while stop.load(Ordering::SeqCst) == 0 {
                tick += 1;
                wclient
                    .execute(
                        &format!(
                            "INSERT INTO {table} (id, payload) VALUES ($1, $2) \
                             ON CONFLICT (id) DO UPDATE SET payload = EXCLUDED.payload"
                        ),
                        &[
                            &(i64::try_from(tick).unwrap()),
                            &format!("write {writer:03} {tick}"),
                        ],
                    )
                    .await
                    .unwrap_or_else(|e| panic!("writer {writer} insert failed: {e}"));
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tick
        }));
    }

    // The pre-take barrier: every writer has certainly committed its next rows before
    // the take is invoked (each commit precedes its own counter increment, so this
    // orders those commits strictly before the take's export).
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let all = committed
            .iter()
            .all(|c| c.load(Ordering::SeqCst) >= COMMITS_BEFORE_TAKE);
        if all {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the writers never got underway"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let committed_before_take: Vec<u64> =
        committed.iter().map(|c| c.load(Ordering::SeqCst)).collect();

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let test_provider = provider();
    let target = database_target(source_db.config(), "tenant-a", source_db.name());
    let snapshot = take(&test_provider, &target, &dest).await;

    // Read the counters the moment the take returned, then stop the writers. Every
    // commit counted here preceded the take's end, hence its export; a restored row
    // beyond any of these counters is a phantom (a post-horizon row in the snapshot).
    let committed_by_take_end: Vec<u64> =
        committed.iter().map(|c| c.load(Ordering::SeqCst)).collect();
    stop.store(1, Ordering::SeqCst);
    for writer in writers {
        writer.await.unwrap();
    }

    // Restore and prove the per-table invariant: a fresh non-existent name (the
    // provider's create+load contract; sweep-safe pattern), a direct connection.
    let restore_name = format!("spot_test_{}", uuid::Uuid::now_v7().simple());
    let restored = test_provider
        .restore(
            &snapshot,
            &restore_target(source_db.config().with_dbname(&restore_name), &restore_name),
            &RestoreOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .unwrap_or_else(|e| panic!("the restore failed: {e}"));
    assert_eq!(restored.location, restore_name);
    let (restored_client, restored_conn) = tokio_postgres::connect(
        &support::credentialed_url(&source_db.config().with_dbname(&restore_name)),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap_or_else(|e| panic!("cannot connect to the restored database: {e}"));
    tokio::spawn(async move {
        let _ = restored_conn.await;
    });

    for writer in 0..WRITERS {
        let table = format!("branch_{writer:03}");
        let ids = table_ids(&restored_client, &table).await;
        let count = u64::try_from(ids.len()).unwrap_or(u64::MAX);
        assert!(
            count >= 8 + committed_before_take[writer],
            "table {table} lost horizon rows: {count} restored, \
             {} committed strictly before the take",
            8 + committed_before_take[writer]
        );
        assert!(
            count <= 8 + committed_by_take_end[writer],
            "table {table} holds post-horizon rows: {count} restored, \
             {} commits had landed by the take's end",
            8 + committed_by_take_end[writer]
        );
        // The gapless prefix: ids 1..=count with no torn gap (the fixture seeds 1..=8
        // once; the writers extend the sequence one id at a time).
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(
                u64::try_from(*id).unwrap_or(u64::MAX),
                u64::try_from(index).unwrap_or(u64::MAX) + 1,
                "table {table} has a torn row sequence: id {id} at position {index}"
            );
        }
    }

    // Cross-table: the uncontended spot_meta table is content-identical (the same
    // table-by-table equality as the quiet round trip, under load).
    let text: String = {
        let rows = restored_client
            .query("SELECT key, value FROM spot_meta ORDER BY key", &[])
            .await
            .unwrap_or_else(|e| panic!("reading restored spot_meta failed: {e}"));
        rows.into_iter()
            .map(|row| {
                let key: String = row.get(0);
                let value: String = row.get(1);
                format!("{key}|{value}\n")
            })
            .collect()
    };
    assert_eq!(
        sha_hex(text.as_bytes()),
        sha_hex(b"catalog_version|1\nfeed_schema|1\n"),
        "spot_meta differs after the restore"
    );
    drop(restored_client);
    drop_database(&source_db.config(), &restore_name).await;
}
