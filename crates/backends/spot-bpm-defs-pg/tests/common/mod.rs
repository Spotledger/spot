//! Shared setup: a scratch database, a connection pool and a prepared store.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

/// A pool of `size` connections on the scratch database.
pub fn pool(db: &TestDb, size: usize) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

/// The fixed instant every test passes in (2023-11-14T22:13:20Z); tests never read a clock.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A prepared store for the default tenant.
pub async fn store(db: &TestDb, size: usize) -> PgDefinitionStore {
    let store = PgDefinitionStore::new(pool(db, size));
    store.prepare().await.unwrap();
    store
}
