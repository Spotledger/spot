//! Shared setup: a scratch database, a bridge on a lazily built pool, and the fixtures.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_tenancy::{ClusterId, DatabaseId, Placement, Region, TenantId};
use spot_tenant_move::{
    MoveEvent, MoveId, MoveMode, MovePlan, MoveRecord, MoveState, TransferTotals,
};
use spot_tenant_move_pg::PgMoveStore;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

static NAMES: AtomicUsize = AtomicUsize::new(0);

pub fn pool_of(config: PgConfig, size: usize) -> Pool {
    let mut config = config;
    config.pool_max_size = size;
    PgBackend::connect_unchecked(config)
        .expect("backend")
        .pool()
}

/// A bridge on its own pool of 6 (in flight 3), and the store on it with its tables created.
pub fn open(config: PgConfig) -> (Arc<Bridge>, PgMoveStore) {
    let pool = pool_of(config, 6);
    let bridge = Arc::new(
        Bridge::new(
            BridgeConfig {
                name: format!("move_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                workers: 2,
                max_in_flight: 3,
                queue_capacity: 8,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(20),
            },
            pool.clone(),
        )
        .expect("bridge"),
    );
    let store = PgMoveStore::new(Arc::clone(&bridge), &pool).expect("store");
    store.prepare().expect("prepare");
    (bridge, store)
}

pub fn tenant(n: u8) -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [n; 10])
}

pub fn placement(n: u8, cluster: &str, database: &str) -> Placement {
    Placement {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(database).unwrap(),
        schema: tenant(n).schema_name(),
    }
}

pub fn record(n: u8) -> MoveRecord {
    let plan = MovePlan::new(
        tenant(n),
        placement(n, "a", "db1"),
        placement(n, "b", "db2"),
        Region::new("eu").unwrap(),
        MoveMode::Offline,
    )
    .unwrap();
    MoveRecord {
        id: MoveId::new(format!("move-{n}")).unwrap(),
        plan,
        state: MoveState::Planned,
        export_next: 0,
        import_next: 0,
        exported: TransferTotals::default(),
        imported: TransferTotals::default(),
        verification: None,
        halt: None,
        revision: 0,
        created_at: 1_000,
        updated_at: 1_000,
        history: vec![MoveEvent {
            state: MoveState::Planned,
            at: 1_000,
        }],
    }
}

/// `record` advanced one forward state at `at`.
pub fn advanced(mut record: MoveRecord, at: u64) -> MoveRecord {
    let next = record.state.next().unwrap();
    record.state = next;
    record.updated_at = at;
    record.history.push(MoveEvent { state: next, at });
    record
}
