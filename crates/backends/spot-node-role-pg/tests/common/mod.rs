//! Shared setup: a scratch database and a pool on it.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

/// A pool of four connections to the scratch database.
pub fn pool(db: &TestDb) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some("node_role_pg_test".to_owned());
    cfg.pool = Some(PoolConfig::new(4));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}
