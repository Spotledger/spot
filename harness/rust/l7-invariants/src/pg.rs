//! A scratch PostgreSQL database and a backend on it, for L7 runs on `pg`.

use std::sync::Arc;
use std::time::Duration;

use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::{ErrorKind, StorageError, StorageResult};

/// A scratch database, dropped with this value, and a backend with a connection pool sized for
/// the workload.
pub struct PgEnv {
    /// The scratch database (use [`TestDb::raw_client`] for seeded corruption).
    pub db: TestDb,
    /// The backend.
    pub backend: Arc<PgBackend>,
    /// Pooled connections actually used.
    pub pool_size: usize,
}

/// Creates a scratch database and a backend for `workers` concurrent workers, or `None` (after a
/// `SKIPPED` notice) when `SPOT_TEST_PG_URL` is not set. With `SPOT_REQUIRE_PG=1` a missing
/// server is a failure instead (`TestDb::create` panics, so a CI run cannot pass by skipping).
///
/// The pool gets one connection per worker plus a few for checks, but no more than the server can
/// spare; workers beyond that queue for connections.
pub async fn pg_env(workers: u32) -> Option<PgEnv> {
    let db = TestDb::create().await?;
    match build(&db, workers).await {
        Ok((backend, pool_size)) => Some(PgEnv {
            db,
            backend: Arc::new(backend),
            pool_size,
        }),
        Err(e) => panic!("cannot connect to the scratch database: {e}"),
    }
}

async fn build(db: &TestDb, workers: u32) -> StorageResult<(PgBackend, usize)> {
    let admin = db.admin_client().await;
    let max: i64 = admin
        .query_one("SELECT current_setting('max_connections')::bigint", &[])
        .await
        .map_err(|e| StorageError::new(ErrorKind::Unavailable, e.to_string()))?
        .get(0);
    let reserved: i64 = admin
        .query_one(
            "SELECT current_setting('superuser_reserved_connections')::bigint",
            &[],
        )
        .await
        .map_err(|e| StorageError::new(ErrorKind::Unavailable, e.to_string()))?
        .get(0);
    // Room for the harness's own connections (admin, raw clients, other test binaries).
    let spare = usize::try_from((max - reserved - 25).max(10)).unwrap_or(10);
    let wanted = workers as usize + 4;
    let size = wanted.min(spare);
    let mut config: PgConfig = db.config();
    config.pool_max_size = size;
    config.pool_wait_timeout = Duration::from_secs(300);
    config.connect_timeout = Duration::from_secs(60);
    Ok((PgBackend::connect(config).await?, size))
}
