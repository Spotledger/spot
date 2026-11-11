//! Shared setup: a scratch database with a bridge, and the table of cases both stores run.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::time::Duration;

use spot_partition::{
    AssignmentRequest, MemoryPartitionRegistry, MemoryRangeAllocator, PartitionRegistry,
    RangeAllocator, ScopeKey,
};
use spot_partition_pg::{PgPartitionRegistry, PgRangeAllocator};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

pub const POOL: usize = 6;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (with a printed message) when no server is configured.
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
    bridge_for(fx.db.config(), "partition_pg_test", in_flight, queue)
}

/// A registry and an allocator on `bridge`, with the tables created.
pub fn pg_stores(bridge: &Arc<Bridge>) -> (PgPartitionRegistry, PgRangeAllocator) {
    let registry = PgPartitionRegistry::new(Arc::clone(bridge), POOL).expect("registry");
    registry.prepare().expect("prepare");
    let allocator = PgRangeAllocator::new(Arc::clone(bridge), POOL).expect("allocator");
    (registry, allocator)
}

pub fn t(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
}

pub fn scope(value: &str) -> ScopeKey {
    ScopeKey::new("warehouse", value).unwrap()
}

pub fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

pub fn request(value: &str, node_name: &str, from: i64, to: Option<i64>) -> AssignmentRequest {
    AssignmentRequest {
        scope: scope(value),
        node: node(node_name),
        effective_from: t(from),
        effective_to: to.map(t),
        actor: "admin".to_owned(),
        reason: "test".to_owned(),
        at: t(from),
    }
}

/// One case of the table both stores run.
pub type Case = fn(&dyn PartitionRegistry, &mut dyn RangeAllocator);

/// Runs `cases` against the in-memory stores (the contract) and, when a server is configured,
/// against PostgreSQL, each case on a fresh database.
pub fn run_on_both(cases: &[(&str, Case)]) {
    for (name, case) in cases {
        let registry = MemoryPartitionRegistry::new();
        let mut allocator = MemoryRangeAllocator::new();
        println!("memory: {name}");
        case(&registry, &mut allocator);
        let Some(fx) = fixture() else { continue };
        let bridge = bridge(&fx, 4, 16);
        let (registry, mut allocator) = pg_stores(&bridge);
        println!("postgres: {name}");
        case(&registry, &mut allocator);
    }
}
