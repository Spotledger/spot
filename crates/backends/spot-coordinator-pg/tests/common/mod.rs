//! Shared test fixtures: a pool over a scratch database and a prepared coordinator.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_coordinator_pg::{PgCoordinator, PgCoordinatorConfig};
use spot_postgres::testing::TestDb;

/// A ttl short enough that a test may wait it out; the contract suite's own is 10 s and it moves
/// a `ManualClock`, which this backend, on the database's clock, cannot.
/// Long enough that a lease taken over just before an assertion is still live when the assertion
/// runs on a loaded runner (600 ms was not; see #840 and #841).
pub const TTL: Duration = Duration::from_millis(2_500);

/// A fresh scratch database, or `None` (printing `SKIPPED`) without `SPOT_TEST_PG_URL`.
pub async fn db() -> Option<TestDb> {
    TestDb::create().await
}

/// A pool on `db`'s scratch database, as the caller of [`PgCoordinator::new`] builds one.
pub fn pool(db: &TestDb, max_size: usize) -> Pool {
    let config = db.config();
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name("spot-coordinator-pg-test");
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .build()
        .expect("the pool builds")
}

/// A coordinator on `db`, with its tables created.
pub async fn coordinator(
    db: &TestDb,
    config: PgCoordinatorConfig,
    max_size: usize,
) -> PgCoordinator {
    let coordinator = PgCoordinator::new(pool(db, max_size), config);
    coordinator.prepare().await.expect("prepare");
    coordinator
}

/// A coordinator with the default configuration.
pub async fn prepared(db: &TestDb) -> PgCoordinator {
    coordinator(db, PgCoordinatorConfig::default(), 8).await
}

/// A node id, or a panic.
pub fn node(id: &str) -> spot_coordinator::NodeId {
    match spot_coordinator::NodeId::new(id) {
        Ok(node) => node,
        Err(error) => panic!("node id {id}: {error}"),
    }
}
