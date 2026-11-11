//! PostgreSQL-specific behaviour: concurrency, rollback, restart, the bridge's refusals, a stopped
//! database and what errors may say. Gated on `SPOT_TEST_PG_URL` (skipped with a message when
//! unset, a failure when `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::sync::Arc;

use common::{POOL, bridge, bridge_for, fixture, node, pg_stores, request, scope, t};
use spot_partition::{PartitionError, PartitionRegistry, RangeAllocator};
use spot_partition_pg::{PgPartitionError, PgPartitionRegistry, PgRangeAllocator};
use spot_storage_contract::SeriesKey;

fn series(key: &str) -> SeriesKey {
    SeriesKey::new(key).unwrap()
}

#[test]
fn fifty_concurrent_assigns_of_one_interval_have_exactly_one_winner() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let (registry, _) = pg_stores(&bridge);
    let registry = Arc::new(registry);
    let mut handles = Vec::new();
    for i in 0..50 {
        let registry = Arc::clone(&registry);
        handles.push(std::thread::spawn(move || {
            registry.assign(request("W1", &format!("edge-{i}"), 0, Some(1_000)))
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(winners, 1, "exactly one winner per interval");
    for result in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(result.code(), "partition.ownership_conflict");
    }
    assert_eq!(registry.history().len(), 1);
    assert_eq!(registry.map_at(t(10)).entries().len(), 1);
}

#[test]
fn two_allocators_granting_a_thousand_times_each_make_disjoint_gapless_ranges() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let (_, first) = pg_stores(&bridge);
    let second = PgRangeAllocator::new(Arc::clone(&bridge), POOL).unwrap();
    let run = |mut allocator: PgRangeAllocator, name: &'static str| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for i in 0..1_000_i64 {
                let size = 1 + u64::try_from(i % 7).unwrap();
                let lease = allocator
                    .lease(&series("SINV-"), &node(name), size, t(i))
                    .unwrap();
                out.push((lease.from, lease.to));
            }
            out
        })
    };
    let (a, b) = (run(first, "edge-a"), run(second, "edge-b"));
    let mut all = a.join().unwrap();
    all.extend(b.join().unwrap());
    assert_eq!(all.len(), 2_000);
    all.sort_unstable();
    assert_eq!(all[0].0, 1, "numbering starts at 1");
    for pair in all.windows(2) {
        assert_eq!(
            pair[1].0,
            pair[0].1 + 1,
            "no gap and no overlap between {pair:?}"
        );
    }
    let (_, reader) = pg_stores(&bridge);
    drop(reader);
    let grants = PgRangeAllocator::new(Arc::clone(&bridge), POOL)
        .unwrap()
        .grants();
    assert_eq!(grants.len(), 2_000);
}

#[test]
fn a_rolled_back_grant_leaves_the_cursor_unchanged() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 16);
    let (_, mut allocator) = pg_stores(&bridge);
    let raw = fx.rt.block_on(fx.db.raw_client());
    // A series used for the first time: its grant collides with a row that is already there.
    fx.rt
        .block_on(raw.execute(
            "INSERT INTO spot_range_grant (series_key, node, first_number, last_number, granted_at) \
             VALUES ('fresh', 'x', 1, 5, 0)",
            &[],
        ))
        .unwrap();
    let failed = allocator
        .lease(&series("fresh"), &node("edge-a"), 3, t(1))
        .unwrap_err();
    assert_eq!(failed.code(), "partition.store");
    let rows = fx
        .rt
        .block_on(raw.query(
            "SELECT 1 FROM spot_range_series WHERE series_key = 'fresh'",
            &[],
        ))
        .unwrap();
    assert!(
        rows.is_empty(),
        "the series row was rolled back with the grant"
    );
    // A series that already has a cursor keeps it.
    let ok = allocator
        .lease(&series("used"), &node("edge-a"), 3, t(2))
        .unwrap();
    assert_eq!((ok.from, ok.to), (1, 3));
    fx.rt
        .block_on(raw.execute(
            "INSERT INTO spot_range_grant (series_key, node, first_number, last_number, granted_at) \
             VALUES ('used', 'x', 4, 9, 0)",
            &[],
        ))
        .unwrap();
    assert!(
        allocator
            .lease(&series("used"), &node("edge-a"), 3, t(3))
            .is_err()
    );
    let cursor: String = fx
        .rt
        .block_on(raw.query_one(
            "SELECT next_free::text FROM spot_range_series WHERE series_key = 'used'",
            &[],
        ))
        .unwrap()
        .get(0);
    assert_eq!(
        cursor, "4",
        "the cursor still names the first number nobody holds a grant for"
    );
}

#[test]
fn history_and_grants_are_append_only() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 16);
    let (registry, mut allocator) = pg_stores(&bridge);
    registry.assign(request("W1", "edge-a", 0, None)).unwrap();
    allocator
        .lease(&series("S"), &node("edge-a"), 5, t(1))
        .unwrap();
    let raw = fx.rt.block_on(fx.db.raw_client());
    for statement in [
        "UPDATE spot_partition_history SET actor = 'mallory'",
        "DELETE FROM spot_partition_history",
        "TRUNCATE spot_partition_history",
        "UPDATE spot_range_grant SET last_number = 99",
        "DELETE FROM spot_range_grant",
        "TRUNCATE spot_range_grant",
    ] {
        assert!(
            fx.rt.block_on(raw.batch_execute(statement)).is_err(),
            "{statement} must be refused"
        );
    }
    assert_eq!(registry.history().len(), 1);
    assert_eq!(allocator.grants().len(), 1);
}

#[test]
fn state_survives_dropping_the_stores_and_preparing_again_is_harmless() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 16);
    {
        let (registry, mut allocator) = pg_stores(&bridge);
        registry
            .assign(request("W1", "edge-a", 0, Some(100)))
            .unwrap();
        allocator
            .lease(&series("S"), &node("edge-a"), 10, t(1))
            .unwrap();
    }
    let (registry, mut allocator) = pg_stores(&bridge);
    assert_eq!(
        registry.map_at(t(5)).owner(&scope("W1")),
        Some(&node("edge-a"))
    );
    assert_eq!(registry.history().len(), 1);
    let next = allocator
        .lease(&series("S"), &node("edge-b"), 10, t(2))
        .unwrap();
    assert_eq!((next.from, next.to), (11, 20));
    assert_eq!(
        registry
            .assign(request("W1", "edge-b", 50, None))
            .unwrap_err()
            .code(),
        "partition.ownership_conflict"
    );
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 16);
    let (registry, mut allocator) = pg_stores(&bridge);
    fx.rt.block_on(async {
        let error = registry.try_map_at(t(0)).unwrap_err();
        assert_eq!(error.code(), "partition.store");
        assert!(format!("{error:?}").contains("pg_bridge.wrong_thread"));
        let error = allocator
            .lease(&series("S"), &node("edge-a"), 1, t(0))
            .unwrap_err();
        assert!(format!("{error:?}").contains("pg_bridge.wrong_thread"));
        // The trait read fails closed and says so.
        assert!(registry.map_at(t(0)).entries().is_empty());
        assert!(registry.take_read_failure().is_some());
        // Building a store here is refused at construction.
        let built = PgPartitionRegistry::new(Arc::clone(&bridge), POOL);
        assert!(matches!(
            built,
            Err(PgPartitionError::UnsupportedContext { .. })
        ));
        assert_eq!(
            built.err().unwrap().code(),
            "partition_pg.unsupported_context"
        );
    });
}

#[test]
fn a_bridge_larger_than_the_pool_is_refused() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 16);
    let error = PgPartitionRegistry::new(Arc::clone(&bridge), 3)
        .err()
        .unwrap();
    assert_eq!(error.code(), "partition_pg.unsupported_context");
    assert!(PgRangeAllocator::new(bridge, 3).is_err());
}

#[test]
fn a_stopped_database_is_an_error_leaves_no_partial_row_and_says_nothing_secret() {
    let Some(fx) = fixture() else { return };
    // The healthy side, to look for rows afterwards.
    let healthy = bridge(&fx, 4, 16);
    let (good_registry, _) = pg_stores(&healthy);
    // The same database, reached on a port nothing listens on.
    let mut dead = fx.db.config();
    let password = dead.password.clone();
    dead.port = 1;
    let dead_bridge = bridge_for(dead, "partition_pg_dead", 4, 16);
    let registry = PgPartitionRegistry::new(Arc::clone(&dead_bridge), POOL).unwrap();
    let mut allocator = PgRangeAllocator::new(dead_bridge, POOL).unwrap();
    let errors = [
        registry
            .assign(request("W1", "edge-a", 0, None))
            .unwrap_err(),
        registry.try_map_at(t(0)).unwrap_err(),
        registry.try_history().unwrap_err(),
        allocator
            .lease(&series("S"), &node("edge-a"), 5, t(0))
            .unwrap_err(),
        allocator.try_grants().unwrap_err(),
    ];
    for error in &errors {
        assert!(
            matches!(error, PartitionError::Store { .. }),
            "got {error:?}"
        );
        for text in [format!("{error:?}"), error.code().to_owned()] {
            assert!(!text.contains("postgres://"), "{text}");
            assert!(
                !text.contains("SELECT") && !text.contains("INSERT"),
                "{text}"
            );
            assert!(
                !text.contains("127.0.0.1") && !text.contains("localhost"),
                "{text}"
            );
            if !password.is_empty() {
                assert!(!text.contains(&password), "{text}");
            }
        }
    }
    // The trait read is empty and fail-closed, and the failure is recorded.
    assert!(registry.map_at(t(0)).entries().is_empty());
    assert!(matches!(
        registry.take_read_failure(),
        Some(PartitionError::Store { .. })
    ));
    assert!(registry.take_read_failure().is_none(), "taking clears it");
    assert!(good_registry.history().is_empty(), "no partial row");
    assert!(good_registry.map_at(t(0)).entries().is_empty());
}
