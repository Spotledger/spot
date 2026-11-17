//! The durable stores on a real database: the leased ranges (restart never reuses a number, a
//! reservation never goes down, two tenants never meet) and the identity state slots. Gated on
//! `SPOT_TEST_PG_URL` (skipped with a message when unset, a failure when `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use spot_edge_offline_host::carrier::LeaseAnswerMsg;
use spot_edge_offline_host::identity_pg::{self, PgIdentityState};
use spot_edge_offline_host::ranges::RangeBook;
use spot_edge_offline_host::series::{RangeView, SeriesPolicy};
use spot_edge_offline_host::store_pg::{self, PgRangeStore};
use spot_offline_identity_adapters::{SealedStateStore, Slot};
use spot_partition::{HighWaterStore, LeaseId, LowWater, RangeLease, StoredSeries};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;

const POOL: usize = 6;
const STRIDE: u64 = 10;

struct Fixture {
    rt: tokio::runtime::Runtime,
    db: TestDb,
}

fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

fn bridge(config: PgConfig) -> Arc<Bridge> {
    let mut config = config;
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("backend");
    Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "edge_offline_test".to_owned(),
                workers: 2,
                max_in_flight: 4,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(10),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .expect("bridge"),
    )
}

fn node() -> NodeId {
    NodeId::new("edge-1#branch_1").unwrap()
}

fn key() -> SeriesKey {
    SeriesKey::new("INV").unwrap()
}

fn open_book(store: PgRangeStore) -> RangeBook {
    RangeBook::open(
        node(),
        vec![SeriesPolicy {
            name: "INV".to_owned(),
            block: 1_000,
        }],
        Arc::new(store),
        LowWater::half(),
    )
    .unwrap()
}

fn grant(from: u64, to: u64, lease_id: u64) -> LeaseAnswerMsg {
    LeaseAnswerMsg::Granted {
        request_id: "r".to_owned(),
        series: "INV".to_owned(),
        from,
        to,
        lease_id,
    }
}

#[test]
fn every_statement_carries_the_tenant() {
    for sql in store_pg::STATEMENTS {
        if !sql.contains("to_regclass") {
            assert!(
                sql.contains("tenant = $1") || sql.trim_start().starts_with("INSERT"),
                "{sql}"
            );
        }
    }
    for sql in identity_pg::STATEMENTS {
        if !sql.contains("to_regclass") {
            assert!(
                sql.contains("tenant = $1") || sql.trim_start().starts_with("INSERT"),
                "{sql}"
            );
        }
    }
    assert!(store_pg::DDL.starts_with("CREATE TABLE IF NOT EXISTS spot_leased_range"));
    assert!(identity_pg::DDL.starts_with("CREATE TABLE IF NOT EXISTS spot_offline_identity_state"));
}

#[test]
fn a_restart_never_reuses_a_number_and_the_host_refuses_a_missing_table() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let bridge = bridge(f.db.config());
    let acme = PgRangeStore::new(Arc::clone(&bridge), "acme", STRIDE);
    let missing = acme.require_table().unwrap_err();
    assert!(missing.contains("spot migrate"));
    f.rt.block_on(async {
        f.db.raw_client()
            .await
            .batch_execute(store_pg::DDL)
            .await
            .unwrap();
    });
    acme.require_table().unwrap();

    let ranges = open_book(acme);
    ranges.install(&grant(1, 1_000, 1)).unwrap();
    let mut taken = Vec::new();
    {
        let sequences = ranges.sequences();
        for _ in 0..25 {
            taken.push(sequences.lock().unwrap().allocate_block(&key(), 1).unwrap());
        }
    }
    assert_eq!(taken, (1..=25).collect::<Vec<u64>>());
    drop(ranges);

    // A crash and a restart: a new store, a new book. The series continues above everything that
    // was handed out; the numbers between the last one and the reservation are a gap.
    let again = open_book(PgRangeStore::new(Arc::clone(&bridge), "acme", STRIDE));
    let first = again
        .sequences()
        .lock()
        .unwrap()
        .allocate_block(&key(), 1)
        .unwrap();
    assert!(first > 25, "{first} must not reuse a handed-out number");
    assert!(first <= 25 + STRIDE + 1, "the gap is bounded by the stride");
    assert_eq!(again.last_held("INV"), 1_000);
    assert!(again.remaining("INV") < 1_000 - 25 + 1);
}

#[test]
fn a_reservation_never_goes_down_and_a_lower_lease_is_refused() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let bridge = bridge(f.db.config());
    f.rt.block_on(async {
        f.db.raw_client()
            .await
            .batch_execute(store_pg::DDL)
            .await
            .unwrap();
    });
    let first = PgRangeStore::new(Arc::clone(&bridge), "acme", STRIDE);
    let lease = RangeLease::new(key(), 1, 1_000, node(), LeaseId::new(1)).unwrap();
    let next = first
        .save_if_above(
            &key(),
            &node(),
            StoredSeries {
                lease: lease.clone(),
                next: 1,
            },
        )
        .unwrap();
    assert_eq!(next, 1);
    first
        .save(
            &key(),
            &node(),
            StoredSeries {
                lease: lease.clone(),
                next: 50,
            },
        )
        .unwrap();

    // A second process view (no cache) trying to write a lower mark is refused, not applied.
    let second = PgRangeStore::new(Arc::clone(&bridge), "acme", STRIDE);
    assert!(
        second
            .save(
                &key(),
                &node(),
                StoredSeries {
                    lease: lease.clone(),
                    next: 5
                }
            )
            .is_err()
    );
    assert!(second.load(&key(), &node()).unwrap().next >= 50);

    // A lease that does not lie above the stored one is refused (the library's rule, in the
    // transaction).
    let overlapping = RangeLease::new(key(), 500, 1_500, node(), LeaseId::new(2)).unwrap();
    assert!(
        second
            .save_if_above(
                &key(),
                &node(),
                StoredSeries {
                    lease: overlapping,
                    next: 500
                }
            )
            .is_err()
    );
    let above = RangeLease::new(key(), 1_001, 2_000, node(), LeaseId::new(3)).unwrap();
    assert_eq!(
        second
            .save_if_above(
                &key(),
                &node(),
                StoredSeries {
                    lease: above,
                    next: 1_001
                }
            )
            .unwrap(),
        1_001
    );
}

#[test]
fn two_tenants_on_one_database_never_share_a_range() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let bridge = bridge(f.db.config());
    f.rt.block_on(async {
        f.db.raw_client()
            .await
            .batch_execute(store_pg::DDL)
            .await
            .unwrap();
    });
    let acme = open_book(PgRangeStore::new(Arc::clone(&bridge), "acme", STRIDE));
    let globex = open_book(PgRangeStore::new(Arc::clone(&bridge), "globex", STRIDE));
    acme.install(&grant(1, 1_000, 1)).unwrap();
    assert_eq!(globex.remaining("INV"), 0);
    globex.install(&grant(5_001, 6_000, 1)).unwrap();
    let a = acme
        .sequences()
        .lock()
        .unwrap()
        .allocate_block(&key(), 1)
        .unwrap();
    let g = globex
        .sequences()
        .lock()
        .unwrap()
        .allocate_block(&key(), 1)
        .unwrap();
    assert_eq!((a, g), (1, 5_001));
    // Reopened, each tenant sees only its own.
    let acme2 = open_book(PgRangeStore::new(Arc::clone(&bridge), "acme", STRIDE));
    assert_eq!(acme2.last_held("INV"), 1_000);
    let globex2 = open_book(PgRangeStore::new(bridge, "globex", STRIDE));
    assert_eq!(globex2.last_held("INV"), 6_000);
}

#[test]
fn the_identity_state_slots_round_trip_per_tenant_and_node() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let bridge = bridge(f.db.config());
    let mut acme = PgIdentityState::new(Arc::clone(&bridge), "acme", "edge-1");
    assert!(acme.require_table().unwrap_err().contains("spot migrate"));
    f.rt.block_on(async {
        f.db.raw_client()
            .await
            .batch_execute(identity_pg::DDL)
            .await
            .unwrap();
    });
    acme.require_table().unwrap();
    assert_eq!(acme.read(Slot::State).unwrap(), None);
    acme.write(Slot::State, b"sealed-state").unwrap();
    acme.write(Slot::Fence, b"sealed-fence").unwrap();
    acme.write(Slot::State, b"sealed-state-2").unwrap();
    assert_eq!(
        acme.read(Slot::State).unwrap().as_deref(),
        Some(&b"sealed-state-2"[..])
    );
    assert_eq!(
        acme.read(Slot::Fence).unwrap().as_deref(),
        Some(&b"sealed-fence"[..])
    );
    let other_tenant = PgIdentityState::new(Arc::clone(&bridge), "globex", "edge-1");
    assert_eq!(other_tenant.read(Slot::State).unwrap(), None);
    let other_node = PgIdentityState::new(bridge, "acme", "edge-2");
    assert_eq!(other_node.read(Slot::State).unwrap(), None);
}
