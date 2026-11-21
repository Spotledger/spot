//! Shared fixtures: a pool over a scratch database, a prepared store, and a recording telemetry.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_audit_store::{PgAuditStore, Telemetry};
use spot_postgres::testing::TestDb;

/// A fresh scratch database, or `None` (printing `SKIPPED`) without `SPOT_TEST_PG_URL`.
pub async fn db() -> Option<TestDb> {
    TestDb::create().await
}

/// A pool on `db`'s scratch database.
pub fn pool(db: &TestDb, max_size: usize) -> Pool {
    let config = db.config();
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name("spot-audit-store-test");
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

/// A prepared store on `db`.
pub async fn store(db: &TestDb) -> PgAuditStore {
    let store = PgAuditStore::new(pool(db, 8));
    store.prepare().await.expect("prepare");
    store
}

/// A raw client on `db`, for the superuser tamper and restore paths.
pub async fn raw(db: &TestDb) -> tokio_postgres::Client {
    db.raw_client().await
}

/// A [`Telemetry`] that counts `audit.dropped`.
#[derive(Clone, Default)]
pub struct Recording {
    dropped: Arc<Mutex<u64>>,
}

impl Recording {
    /// A new recording telemetry.
    #[must_use]
    pub fn new() -> Recording {
        Recording::default()
    }

    /// How many `audit.dropped` counts it saw.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        *self.dropped.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Telemetry for Recording {
    fn counter(&self, name: &str, by: u64) {
        if name == "audit.dropped" {
            *self.dropped.lock().unwrap_or_else(|e| e.into_inner()) += by;
        }
    }
}
