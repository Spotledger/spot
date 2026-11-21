//! The handle every store shares: the bridge and the checks a constructor makes.

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig, BridgeError, PooledConn};

use crate::error::{OpError, PgNodeError, map};
use crate::schema;

/// Deadline of the table creation and of the missing-table check.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The longest tenant name the tables accept (the schema checks it too).
pub(crate) const MAX_NAME: usize = 128;

/// The shared bridge. Cheap to clone. Tenancy is not here: every statement takes its tenant as a
/// parameter and filters on it.
#[derive(Clone)]
pub struct Db {
    pub(crate) bridge: Arc<Bridge>,
}

impl std::fmt::Debug for Db {
    /// Shows no connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

/// Whether `tenant` is a name the tables accept.
pub(crate) fn valid_tenant(tenant: &str) -> bool {
    !tenant.is_empty() && tenant.len() <= MAX_NAME && !tenant.chars().any(char::is_control)
}

impl Db {
    /// A handle on `bridge`. `pool_size` is the size of the pool the bridge was built on; no I/O
    /// happens here.
    ///
    /// # Errors
    /// `sync_node_pg.unsupported_context` when the thread is inside a tokio runtime, or the bridge
    /// allows more calls in flight than the pool can serve.
    pub fn new(bridge: Arc<Bridge>, pool_size: usize) -> Result<Self, PgNodeError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(PgNodeError::UnsupportedContext {
                reason: "the store is built inside an async runtime; build it on a plain thread",
            });
        }
        if bridge.config().max_in_flight > BridgeConfig::largest_in_flight(pool_size) {
            return Err(PgNodeError::UnsupportedContext {
                reason: "the bridge allows more calls in flight than the pool can serve",
            });
        }
        Ok(Self { bridge })
    }

    /// Creates every table (idempotent).
    ///
    /// # Errors
    /// `sync_node_pg.prepare_failed` when the DDL cannot be applied or the bridge refuses.
    pub fn prepare(&self) -> Result<(), PgNodeError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn| {
                Box::pin(async move { schema::prepare(&mut conn).await })
            })
            .map_err(|_| PgNodeError::PrepareFailed)
    }

    /// The node tables that do not exist. The host refuses to receive while this is not empty.
    ///
    /// # Errors
    /// The store's failure; a database that cannot answer is not "no tables missing".
    pub fn missing_tables(&self) -> Result<Vec<&'static str>, PgNodeError> {
        self.bridge
            .run(PREPARE_DEADLINE, |conn| {
                Box::pin(async move { schema::missing(&conn).await })
            })
            .map_err(map)
    }

    /// Fails with `sync_node_pg.tables_missing` when any table is missing.
    ///
    /// # Errors
    /// `sync_node_pg.tables_missing`, or the store's failure.
    pub fn require_tables(&self) -> Result<(), PgNodeError> {
        let tables = self.missing_tables()?;
        if tables.is_empty() {
            Ok(())
        } else {
            Err(PgNodeError::TablesMissing { tables })
        }
    }

    /// Runs `op` on a pooled connection with `deadline`.
    pub(crate) fn run<T, F>(&self, deadline: Duration, op: F) -> Result<T, BridgeError<OpError>>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> spot_pg_bridge::BoxFuture<'static, Result<T, OpError>>
            + Send
            + 'static,
    {
        self.bridge.run(deadline, op)
    }
}
