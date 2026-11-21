//! Shared setup: a scratch database, a bridge on a lazily built pool, and the store with its
//! tables created. The shared conformance cases come from the port crate's own tests.
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
use spot_store_state_pg::PgStoreState;

#[path = "../../../../framework/spot-store-state/tests/common/conformance.rs"]
pub mod conformance;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// `None` (a skip, after a message) when no server is configured; a failure when
/// `SPOT_REQUIRE_PG=1`.
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

pub fn bridge_config() -> BridgeConfig {
    BridgeConfig {
        name: format!("store_state_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
        workers: 2,
        max_in_flight: 6,
        queue_capacity: 32,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    }
}

/// A bridge on its own pool of 10 (in flight 6), and the store on it with its tables created.
pub fn open_db(config: PgConfig) -> (Arc<Bridge>, Arc<PgStoreState>) {
    let pool = pool_of(config, 10);
    let bridge = Arc::new(Bridge::new(bridge_config(), pool.clone()).expect("bridge"));
    let store = Arc::new(PgStoreState::new(Arc::clone(&bridge), &pool).expect("store"));
    store.prepare().expect("prepare");
    (bridge, store)
}
