//! [`PgBackend`]: the pool, capabilities, health and `begin`.
//!
//! [`PgBackend::pool`] is how the sibling PostgreSQL stores of the same site get their pool: a
//! handle to this backend's pool, so a site has one pool and one health check, never two.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use deadpool_postgres::{Object, Pool, PoolError};
use spot_storage_contract::{
    CONTRACT_VERSION, Capabilities, Catalog, ChangeFeed, ErrorKind, HealthCheck, HealthReport,
    HealthStatus, Query, ReadSource, Rows, StorageBackend, StorageError, StorageResult, TxOptions,
};
use tokio_postgres::SimpleQueryMessage;

use crate::config::PgConfig;
use crate::conn::{TlsMode, build_pool, connect_raw};
use crate::error::map_pg_error;
use crate::{SUPPORTED_MAJOR_VERSION, feed, query, schema, tx};

/// The highest `max_writes` a transaction may declare on this backend.
const MAX_WRITES_PER_TX: u32 = 10_000;

/// How long a queued waiter may wait before the pool is reported `Degraded`.
const POOL_WAIT_DEGRADED_AFTER: Duration = Duration::from_secs(1);

/// One tenant's PostgreSQL database behind a connection pool. Cheap to clone and share.
#[derive(Clone)]
pub struct PgBackend {
    inner: Arc<Inner>,
}

struct Inner {
    config: PgConfig,
    /// Built once, in [`PgBackend::build`], and used by the pool, by the raw connections of the
    /// health check and by [`PgBackend::cancel_tls`], so they all agree on the transport.
    tls: TlsMode,
    pool: Pool,
    /// `server_version` as reported when connecting; unset for an unchecked backend.
    server_version: OnceLock<String>,
    waiters: Waiters,
}

impl PgBackend {
    /// Opens the pool and checks the server: it must be PostgreSQL 18 and offer the ICU collation
    /// provider (needed from WP-STO-03 on). Anything else fails with `Unsupported`; an unreachable
    /// server fails with `Unavailable`.
    pub async fn connect(config: PgConfig) -> StorageResult<PgBackend> {
        let backend = Self::build(config)?;
        let client = backend.checkout().await?;
        let version = verify_server(&client).await?;
        drop(client);
        // Only this call ever sets it, once.
        let _ = backend.inner.server_version.set(version);
        Ok(backend)
    }

    /// Builds the backend without contacting the server or checking its version.
    ///
    /// For tools and tests that must hold a backend for a server that may be down (for example
    /// to see `health()` report `Down`). Production code uses [`PgBackend::connect`].
    pub fn connect_unchecked(config: PgConfig) -> StorageResult<PgBackend> {
        Self::build(config)
    }

    fn build(config: PgConfig) -> StorageResult<PgBackend> {
        config.validate()?;
        let tls = TlsMode::new(config.tls)?;
        let pool = build_pool(&config, &tls)?;
        Ok(PgBackend {
            inner: Arc::new(Inner {
                config,
                tls,
                pool,
                server_version: OnceLock::new(),
                waiters: Waiters::default(),
            }),
        })
    }

    /// The config this backend was built from.
    pub fn config(&self) -> &PgConfig {
        &self.inner.config
    }

    /// A handle to this backend's connection pool (a cheap clone of the same pool) for the
    /// sibling PostgreSQL stores of the same site, so one site has one pool and one health check.
    /// Use [`PgBackend::begin`] for ordinary work.
    ///
    /// The pool is shared, not a second one: do not close it, and a caller that takes connections
    /// from it competes with this backend for `max_size`. Dropping the handle leaves the pool and
    /// the backend untouched.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.inner.pool.clone()
    }

    /// The TLS connector this backend's pool was built with, for a request that must travel over
    /// the same transport as the pool: `spot-pg-bridge` hands it to `Bridge::with_cancel_tls`, so
    /// the cancel request of a timed-out statement is sent over TLS instead of in clear, and the
    /// connection's cancel key never crosses the network unprotected.
    ///
    /// `None` when the config does not require TLS (`Tls::Disable`): a cancel
    /// request without TLS is then correct, because the pool itself is plain. `Some` is a cheap
    /// clone of the very connector the pool uses — the same client config and the same root
    /// store — so the pool and the cancel request can never disagree about certificates or
    /// verification. A clone opens no connection and reads no file.
    ///
    /// The connector is a type of a dependency of this crate, not of this crate: a caller that
    /// must name it (`tokio_postgres_rustls::MakeRustlsConnect`) depends on
    /// `tokio-postgres-rustls` itself. This crate re-exports nothing of it.
    #[must_use]
    pub fn cancel_tls(&self) -> Option<tokio_postgres_rustls::MakeRustlsConnect> {
        self.inner.tls.connector()
    }

    /// Takes a connection from the pool, waiting at most `pool_wait_timeout`.
    pub(crate) async fn checkout(&self) -> StorageResult<Object> {
        let _waiting = self.inner.waiters.enter();
        self.inner
            .pool
            .get()
            .await
            .map_err(|e| map_pool_error(&e, self.inner.config.pool_wait_timeout))
    }

    async fn probe_primary(&self) -> Result<(Duration, String), String> {
        let cfg = &self.inner.config;
        let started = Instant::now();
        let probe = async {
            let client = connect_raw(cfg, &self.inner.tls)
                .await
                .map_err(|e| e.explanation)?;
            first_value(&client, "SHOW server_version")
                .await
                .map_err(|e| e.explanation)
        };
        match tokio::time::timeout(cfg.connect_timeout, probe).await {
            Ok(Ok(version)) => Ok((started.elapsed(), version)),
            Ok(Err(why)) => Err(why),
            Err(_) => Err(format!(
                "no answer within the connect timeout of {} ms",
                cfg.connect_timeout.as_millis()
            )),
        }
    }
}

impl StorageBackend for PgBackend {
    type Tx = tx::PgTransaction;

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "postgres".to_owned(),
            contract_version: CONTRACT_VERSION.to_owned(),
            read_sources: vec![ReadSource::Primary],
            max_writes_per_tx: MAX_WRITES_PER_TX,
            // Provisional until WP-STO-08 builds the outbox and feed tables and their pruning.
            feed_retention: Duration::from_secs(7 * 24 * 60 * 60),
            // ADR-087: `prepare` creates geo and range columns and exclusions, and refuses with
            // `InvalidSchema` (naming `extension.install`) a database without the extension a
            // catalog needs, so these hold whenever `prepare` succeeds. Search stays off until
            // its projection exists (WP-STO-13.T1).
            geo_filters: true,
            range_exclusion: true,
            search: false,
        }
    }

    async fn prepare(&self, catalog: &Catalog) -> StorageResult<()> {
        schema::prepare(self, catalog).await
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<Self::Tx> {
        tx::begin(self, catalog, options, MAX_WRITES_PER_TX).await
    }

    async fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> StorageResult<Rows> {
        query::read(self, catalog, source, query).await
    }

    async fn health(&self) -> HealthReport {
        let mut checks = Vec::with_capacity(3);
        let version = match self.probe_primary().await {
            Ok((latency, version)) => {
                checks.push(HealthCheck {
                    name: "primary_connection".to_owned(),
                    status: HealthStatus::Healthy,
                    detail: format!("round trip took {} ms", latency.as_millis()),
                });
                Some(version)
            }
            Err(why) => {
                checks.push(HealthCheck {
                    name: "primary_connection".to_owned(),
                    status: HealthStatus::Down,
                    detail: format!("the primary could not be reached: {why}"),
                });
                None
            }
        };
        checks.push(self.pool_check());
        checks.push(match version {
            Some(v) if major_of(&v) == Some(SUPPORTED_MAJOR_VERSION) => HealthCheck {
                name: "server_version".to_owned(),
                status: HealthStatus::Healthy,
                detail: format!("PostgreSQL {v}"),
            },
            Some(v) => HealthCheck {
                name: "server_version".to_owned(),
                status: HealthStatus::Degraded,
                detail: format!("PostgreSQL {v}; only {SUPPORTED_MAJOR_VERSION} is supported"),
            },
            None => HealthCheck {
                name: "server_version".to_owned(),
                status: HealthStatus::Down,
                detail: format!(
                    "unknown (last seen: {})",
                    self.inner
                        .server_version
                        .get()
                        .map_or("never connected", String::as_str)
                ),
            },
        });
        let status = checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(HealthStatus::Healthy);
        HealthReport { status, checks }
    }
}

impl ChangeFeed for PgBackend {
    async fn read_outbox(
        &self,
        after: spot_storage_contract::FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<spot_storage_contract::OutboxRecord>> {
        feed::read_outbox(self, after, limit).await
    }

    async fn read_changes(
        &self,
        after: spot_storage_contract::FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<spot_storage_contract::Change>> {
        feed::read_changes(self, after, limit).await
    }
}

impl PgBackend {
    fn pool_check(&self) -> HealthCheck {
        let status = self.inner.pool.status();
        let idle = status.available;
        let in_use = status.size.saturating_sub(idle);
        let queued = self.inner.waiters.oldest_wait();
        let (state, extra) = match queued {
            Some(wait) if wait > POOL_WAIT_DEGRADED_AFTER => (
                HealthStatus::Degraded,
                format!(
                    "; a caller has waited {} ms for a connection",
                    wait.as_millis()
                ),
            ),
            _ => (HealthStatus::Healthy, String::new()),
        };
        HealthCheck {
            name: "pool".to_owned(),
            status: state,
            detail: format!(
                "{in_use} in use, {idle} idle, {} waiting, at most {}{extra}",
                status.waiting, status.max_size
            ),
        }
    }
}

/// Tracks callers queued for a connection and since when.
#[derive(Default)]
struct Waiters {
    next: Mutex<(u64, BTreeMap<u64, Instant>)>,
}

/// Registers a waiter until dropped.
struct WaitGuard<'a> {
    waiters: &'a Waiters,
    id: u64,
}

impl Waiters {
    fn lock(&self) -> MutexGuard<'_, (u64, BTreeMap<u64, Instant>)> {
        // The map stays valid even if a holder panicked.
        self.next.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn enter(&self) -> WaitGuard<'_> {
        let mut guard = self.lock();
        let id = guard.0;
        guard.0 = id.wrapping_add(1);
        guard.1.insert(id, Instant::now());
        WaitGuard { waiters: self, id }
    }

    fn oldest_wait(&self) -> Option<Duration> {
        self.lock().1.values().map(Instant::elapsed).max()
    }
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.waiters.lock().1.remove(&self.id);
    }
}

fn map_pool_error(err: &PoolError, waited: Duration) -> StorageError {
    match err {
        PoolError::Timeout(_) => StorageError::new(
            ErrorKind::Unavailable,
            format!(
                "no database connection became free within {} ms",
                waited.as_millis()
            ),
        ),
        PoolError::Backend(e) => map_pg_error(e, None),
        PoolError::Closed => {
            StorageError::new(ErrorKind::Unavailable, "the connection pool is closed")
        }
        _ => StorageError::new(
            ErrorKind::Internal,
            "the connection pool failed unexpectedly",
        ),
    }
}

/// The major version of a `server_version` string such as `18.1 (Debian 18.1-1.pgdg120+2)`.
fn major_of(version: &str) -> Option<u32> {
    version.split(['.', ' ']).next()?.parse().ok()
}

/// The first column of the first row of a text query.
async fn first_value(client: &tokio_postgres::Client, sql: &str) -> StorageResult<String> {
    let messages = client
        .simple_query(sql)
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    messages
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(r) => r.get(0).map(str::to_owned),
            _ => None,
        })
        .ok_or_else(|| StorageError::new(ErrorKind::Internal, "the server returned no row"))
}

/// Refuses servers other than PostgreSQL 18 and servers without ICU; returns `server_version`.
async fn verify_server(client: &tokio_postgres::Client) -> StorageResult<String> {
    let version = first_value(client, "SHOW server_version").await?;
    if major_of(&version) != Some(SUPPORTED_MAJOR_VERSION) {
        return Err(StorageError::new(
            ErrorKind::Unsupported,
            format!(
                "the server runs PostgreSQL {version}; Spotledger requires PostgreSQL {SUPPORTED_MAJOR_VERSION}"
            ),
        ));
    }
    let icu = first_value(
        client,
        "SELECT count(*) FROM pg_collation WHERE collprovider = 'i'",
    )
    .await?;
    if icu.parse::<u64>().unwrap_or(0) == 0 {
        return Err(StorageError::new(
            ErrorKind::Unsupported,
            "the server was built without ICU collation support, which Spotledger requires",
        ));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_major_version() {
        assert_eq!(major_of("18.1 (Debian 18.1-1.pgdg120+2)"), Some(18));
        assert_eq!(major_of("18.0"), Some(18));
        assert_eq!(major_of("16.14 (Ubuntu 16.14-0ubuntu0.24.04.1)"), Some(16));
        assert_eq!(major_of("garbage"), None);
    }

    #[test]
    fn oldest_waiter_is_tracked_and_released() {
        let w = Waiters::default();
        assert!(w.oldest_wait().is_none());
        let a = w.enter();
        assert!(w.oldest_wait().is_some());
        drop(a);
        assert!(w.oldest_wait().is_none());
    }
}
