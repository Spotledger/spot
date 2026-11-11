//! Shared setup for the tests: a scratch database, pools and namespaced keys.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Object, Pool, PoolConfig, Runtime};
use spot_postgres::testing::TestDb;
use spot_sso_pg::PgReplayStore;
use tokio_postgres::NoTls;

/// The pool size the contract suite needs: its racing case claims from sixteen plain threads at
/// once, and a thread that has to create a connection has no reactor on which to start it.
pub const POOL: usize = 16;

/// A pool of `size` connections to the scratch database.
pub fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

/// A prepared store on a new pool of `size` connections, in `namespace`.
pub async fn store(db: &TestDb, size: usize, namespace: &str, app: &str) -> PgReplayStore {
    let store = PgReplayStore::new(pool(db, size, app), namespace);
    store.prepare().await.unwrap();
    store
}

/// Opens `n` connections and hands them straight back, so the pool holds them.
pub async fn warm(pool: &Pool, n: usize) {
    let mut held: Vec<Object> = Vec::with_capacity(n);
    for _ in 0..n {
        held.push(pool.get().await.unwrap());
    }
}

/// The store key of `id` of `kind` under `provider`, built the way the verifiers build it.
pub fn namespaced(kind: &str, provider: &str, id: &str) -> String {
    spot_sso::namespaced(kind, provider, id)
}
