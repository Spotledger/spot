//! The catch-up director over the real durable progress table (WP-RTB-02.T7): progress written by
//! one director is resumed by another over the same database, the table is checked, and the
//! bounds of the configuration hold. The routing and skew scenarios are in
//! `spot-sync-catchup-host`'s own tests with virtual time.
//!
//! Skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, Collation, Isolation, Name, StorageBackend, Transaction, TxOptions,
};
use spot_sync_catchup::{ClassCounts, GapSummary};
use spot_sync_catchup_host::{
    CatchupConfig, ChunkAck, Director, PgProgress, ProgressStore, Route, pg::DDL,
};

async fn env() -> Option<(TestDb, PgBackend, Arc<Bridge>)> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    // `spot migrate` creates the table from `schema::extra_tables`.
    let mut options = TxOptions::new(Name::new("catchup_ddl").unwrap(), Isolation::ReadCommitted);
    options.max_writes = 100;
    let mut tx = backend.begin(Arc::clone(&catalog), options).await.unwrap();
    for statement in DDL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        tx.raw_execute_with(PgTransaction::READ, statement, &[])
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let for_bridge = backend.clone();
    let bridge = std::thread::spawn(move || {
        let config = BridgeConfig {
            name: "catchup_test".to_owned(),
            workers: 2,
            max_in_flight: 4,
            queue_capacity: 16,
            default_deadline: Duration::from_secs(10),
            max_deadline: Duration::from_secs(30),
        };
        Bridge::for_backend(config, &for_bridge).unwrap()
    })
    .join()
    .unwrap();
    Some((db, backend, Arc::new(bridge)))
}

fn new_node() -> GapSummary {
    GapSummary {
        applied_prefix: 0,
        stream_end: 5_000,
        hot_retention_floor: 1,
        in_scope_event_count: 5_000,
        class_counts: ClassCounts {
            state: 5_000,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 0,
        estimated_coalesced_bytes: 0,
        estimated_snapshot_bytes: 50_000_000,
        node_is_new_or_resync: true,
    }
}

/// Runs `work` on a plain thread: the blocking bridge to PostgreSQL refuses a call made from a
/// runtime thread, `block_in_place` included.
fn off_runtime<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(work).join().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bootstrap_resumes_from_the_stored_chunk_after_a_restart() {
    let Some((_db, _backend, bridge)) = env().await else {
        return;
    };
    let config = CatchupConfig::default();
    let store: Arc<PgProgress> = Arc::new(PgProgress::new(Arc::clone(&bridge)));
    let first = Director::new(&config, store.clone()).unwrap();
    let route = off_runtime(|| first.route("edge-1", &new_node(), 0)).unwrap();
    assert_eq!(
        route,
        Route::Bootstrap {
            resume_from_chunk: 0
        }
    );
    off_runtime(|| {
        first.plan_cut("edge-1", 10).unwrap();
        for index in 0..6 {
            let ack = first
                .chunk_done("edge-1", index, i64::try_from(index).unwrap() * 1_000)
                .unwrap();
            assert!(matches!(ack, ChunkAck::Next | ChunkAck::Wait(_)));
        }
    });
    // The hub restarts: a new director over a new bridge handle on the same table.
    let second = Director::new(&config, Arc::new(PgProgress::new(bridge))).unwrap();
    let resumed = off_runtime(|| second.route("edge-1", &new_node(), 60_000)).unwrap();
    assert_eq!(
        resumed,
        Route::Bootstrap {
            resume_from_chunk: 6
        }
    );
    off_runtime(|| {
        assert_eq!(
            second.status_text("edge-1").unwrap(),
            "bootstrapping 60% (chunk 6 of 10)"
        );
        second.finish("edge-1").unwrap();
        assert!(store.load("edge-1").unwrap().is_none());
    });
}

#[test]
fn the_thresholds_have_the_briefs_defaults_and_bounds() {
    let config = CatchupConfig::default();
    assert_eq!(config.catchup_threshold_events, 5_000);
    assert_eq!(config.max_parallel_bootstraps, 4);
    assert!(config.validate().is_ok());
}
