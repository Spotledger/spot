//! Shared setup for the tests: a scratch database, pools, a manual clock and stores.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_core::ManualClock;
use spot_http::api::idempotency::fingerprint;
use spot_http::api::{KeyScope, StoredResponse};
use spot_idempotency_pg::{PgIdempotencyConfig, PgIdempotencyStore, RetentionPolicy};
use spot_postgres::testing::TestDb;
use time::macros::datetime;
use tokio_postgres::NoTls;

/// The lease every test store uses unless it says otherwise: two minutes.
pub const LEASE: Duration = Duration::from_secs(120);

/// A clock that starts on a fixed instant of 2026 and moves only when a test says so.
pub fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-06 08:00:00 UTC)))
}

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

/// The standard configuration: a two-minute lease, 24 h default retention, 7 days maximum.
pub fn config() -> PgIdempotencyConfig {
    PgIdempotencyConfig::new(LEASE, RetentionPolicy::standard())
}

/// A store on `pool` with `config`, timed by `clock`; the table is prepared.
pub async fn store_on(
    pool: Pool,
    clock: &Arc<ManualClock>,
    config: PgIdempotencyConfig,
) -> PgIdempotencyStore {
    let store = PgIdempotencyStore::new(pool, clock.clone(), config).unwrap();
    store.prepare().await.unwrap();
    store
}

/// A prepared store on a new pool of `size` connections.
pub async fn store(
    db: &TestDb,
    size: usize,
    app: &str,
    clock: &Arc<ManualClock>,
) -> PgIdempotencyStore {
    store_on(pool(db, size, app), clock, config()).await
}

pub fn scope(credential: &str, key: &str) -> KeyScope {
    KeyScope {
        site: "acme".to_owned(),
        credential: credential.to_owned(),
        key: key.to_owned(),
    }
}

/// A fingerprint the way the router builds one.
pub fn fp(body: &str) -> String {
    fingerprint("POST", "/api/v1/resource/customer", body.as_bytes())
}

pub fn response(status: u16, body: &[u8]) -> StoredResponse {
    StoredResponse {
        status,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("etag".to_owned(), "\"1\"".to_owned()),
        ],
        body: body.to_vec(),
    }
}

/// How many rows the table holds in total.
pub async fn row_count(db: &TestDb) -> i64 {
    db.raw_client()
        .await
        .query_one("SELECT count(*) FROM spot_idempotency_key", &[])
        .await
        .unwrap()
        .get(0)
}

/// The Unix second at which the row of `key` stops being replayed.
pub async fn expires_at(db: &TestDb, key: &str) -> Option<i64> {
    db.raw_client()
        .await
        .query_one(
            "SELECT expires_at FROM spot_idempotency_key WHERE key = $1",
            &[&key],
        )
        .await
        .unwrap()
        .get(0)
}
