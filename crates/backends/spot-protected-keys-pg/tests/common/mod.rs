//! Shared setup: a scratch database, a bridge on a lazily built pool and the stores.
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
use spot_protected_keys_pg::PgKeyStore;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database. With `SPOT_REQUIRE_PG=1` a missing server panics inside `TestDb::create`,
/// so CI cannot go green by skipping; otherwise `None` after a `SKIPPED:` line.
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

/// A bridge on its own pool of 12 (in flight 8).
pub fn open_bridge(config: PgConfig) -> (Arc<Bridge>, Pool) {
    let pool = pool_of(config, 12);
    let bridge = Arc::new(
        Bridge::new(
            BridgeConfig {
                name: format!("protected_keys_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                workers: 2,
                max_in_flight: 8,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(20),
            },
            pool.clone(),
        )
        .expect("bridge"),
    );
    (bridge, pool)
}

/// A store for `tenant` on a fresh bridge, with its tables created.
pub fn open_store(config: PgConfig, tenant: &str) -> (Arc<Bridge>, Pool, Arc<PgKeyStore>) {
    let (bridge, pool) = open_bridge(config);
    let store = Arc::new(PgKeyStore::new(Arc::clone(&bridge), &pool, tenant).expect("store"));
    store.prepare().expect("prepare");
    (bridge, pool, store)
}
