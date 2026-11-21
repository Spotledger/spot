//! The [`PgCoordinator`] and its configuration.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Object, Pool};
use spot_coordinator::coordinator::BoxFuture;
use spot_coordinator::{Admit, Coordinator, FencingToken, Lease, RateLimit, Result};
use tokio::sync::Notify;

use crate::cleanup::{CleanupReport, cleanup};
use crate::presence::{DEFAULT_HEARTBEAT, PgPresence};
use crate::{counters, leases, rate, schema};

/// How a [`PgCoordinator`] times node presence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgCoordinatorConfig {
    /// How often the host loop calls the presence heartbeat.
    pub heartbeat_interval: Duration,
    /// How long a node is considered live after its last heartbeat (default: three intervals).
    pub node_window: Duration,
}

impl Default for PgCoordinatorConfig {
    fn default() -> PgCoordinatorConfig {
        PgCoordinatorConfig {
            heartbeat_interval: DEFAULT_HEARTBEAT,
            node_window: PgPresence::window_for(DEFAULT_HEARTBEAT),
        }
    }
}

impl PgCoordinatorConfig {
    /// The config for a heartbeat interval, with the default three-interval window.
    #[must_use]
    pub fn with_heartbeat(heartbeat: Duration) -> PgCoordinatorConfig {
        PgCoordinatorConfig {
            heartbeat_interval: heartbeat,
            node_window: PgPresence::window_for(heartbeat),
        }
    }
}

/// The PostgreSQL [`Coordinator`]: every call is one pooled, auto-committed statement.
pub struct PgCoordinator {
    pool: Pool,
    config: PgCoordinatorConfig,
    waker: Arc<Notify>,
}

impl PgCoordinator {
    /// A coordinator over `pool`, using `config`. Call [`PgCoordinator::prepare`] once before
    /// use; the pool belongs to the caller and is never used for a business transaction.
    #[must_use]
    pub fn new(pool: Pool, config: PgCoordinatorConfig) -> PgCoordinator {
        PgCoordinator {
            pool,
            config,
            waker: Arc::new(Notify::new()),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &PgCoordinatorConfig {
        &self.config
    }

    /// Creates this backend's tables if the database has none; idempotent and safe to call from
    /// every node at once.
    ///
    /// # Errors
    ///
    /// `coordinator.unavailable` when the database or its pool fails.
    pub async fn prepare(&self) -> Result<()> {
        schema::prepare(&self.pool).await
    }

    /// Deletes expired leases, fully refilled rate buckets and node rows past the presence
    /// window. A scheduled job calls this (WP-PLT-06); the coordinator runs no timer of its own.
    ///
    /// # Errors
    ///
    /// `coordinator.unavailable` when the database or its pool fails.
    pub async fn cleanup(&self) -> Result<CleanupReport> {
        cleanup(&self.pool, self.config.node_window).await
    }

    /// A presence over the same pool, for the in-process coordinator's second-node detection.
    #[must_use]
    pub fn presence(&self) -> PgPresence {
        PgPresence::new(self.pool.clone(), self.config.node_window)
    }

    /// A pooled connection for one statement.
    async fn client(&self, what: &str) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|e| crate::error::pool(what, &e))
    }
}

impl Coordinator for PgCoordinator {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Lease>> {
        Box::pin(async move {
            let client = self.client("acquire").await?;
            leases::acquire(&**client, name, holder, ttl).await
        })
    }

    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, Result<Lease>> {
        Box::pin(async move {
            let client = self.client("renew").await?;
            leases::renew(&**client, lease, ttl).await
        })
    }

    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let client = self.client("release").await?;
            leases::release(&**client, lease).await?;
            self.waker.notify_waiters();
            Ok(())
        })
    }

    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Lease>>> {
        Box::pin(async move {
            let client = self.client("current").await?;
            leases::current(&**client, name).await
        })
    }

    fn check_fence<'a>(&'a self, name: &'a str, token: FencingToken) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let client = self.client("check_fence").await?;
            leases::check_fence(&**client, name, token).await
        })
    }

    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let client = self.client("next").await?;
            counters::next(&**client, name, by).await
        })
    }

    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let client = self.client("peek").await?;
            counters::peek(&**client, name).await
        })
    }

    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let client = self.client("set_at_least").await?;
            counters::set_at_least(&**client, name, value).await
        })
    }

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, Result<Admit>> {
        Box::pin(async move {
            let client = self.client("take").await?;
            rate::take(&**client, key, limit, cost).await
        })
    }

    fn waker(&self) -> Arc<Notify> {
        Arc::clone(&self.waker)
    }
}
