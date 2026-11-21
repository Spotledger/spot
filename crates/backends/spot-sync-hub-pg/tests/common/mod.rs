//! Shared setup: a scratch database with a bridge, and the fixtures the tests build items from.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_sync_closure::{Bundle, BundleKind};
use spot_sync_hub::QueuedBundle;
use spot_sync_hub_pg::PgHub;
use spot_sync_obligations::{Lane, Pending};

pub const POOL: usize = 6;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (with a printed message) when no server is configured; with
/// `SPOT_REQUIRE_PG=1` a missing server fails.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

pub fn bridge_for(config: PgConfig, name: &str, in_flight: usize, queue: usize) -> Arc<Bridge> {
    let mut config = config;
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("backend");
    Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: name.to_owned(),
                workers: 2,
                max_in_flight: in_flight,
                queue_capacity: queue,
                default_deadline: Duration::from_secs(10),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .expect("bridge"),
    )
}

pub fn bridge(fx: &Fixture, in_flight: usize, queue: usize) -> Arc<Bridge> {
    bridge_for(fx.db.config(), "sync_hub_pg_test", in_flight, queue)
}

/// A prepared hub for `tenant` on `bridge`.
pub fn hub(bridge: &Arc<Bridge>, tenant: &str) -> PgHub {
    let hub = PgHub::new(Arc::clone(bridge), POOL, tenant).expect("hub");
    hub.prepare().expect("prepare");
    hub
}

/// A queue item: an empty root bundle of `bytes` bytes.
pub fn item(
    lane: Lane,
    position: u64,
    index: u32,
    redelivery: bool,
    bytes: usize,
) -> Pending<QueuedBundle> {
    Pending {
        lane,
        position,
        item: QueuedBundle {
            index,
            redelivery,
            bundle: Bundle {
                kind: BundleKind::Root,
                records: Vec::new(),
                bytes,
            },
            serve_seq: 0,
            serve_after: 0,
            floor: 0,
        },
    }
}
