//! The handle every store shares: the bridge, the tenant and the checks a constructor makes.

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};

use crate::error::{OpError, PgHubError, map};
use crate::queue::{PgSendQueue, QueueLimits};
use crate::{PgAckBook, PgNodeRegistry, PgNodeStateStore, PgRelayStore, schema};

/// Deadline of the table creation and of the missing-table check.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The longest tenant, node or destination name the tables accept (the schema checks it too).
pub(crate) const MAX_NAME: usize = 128;

/// The bridge and the tenant one store works for. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) tenant: String,
}

impl std::fmt::Debug for Db {
    /// Names the tenant, never a connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl Db {
    /// A handle on `bridge` for `tenant`. `pool_size` is the size of the pool the bridge was
    /// built on; no I/O happens here.
    ///
    /// # Errors
    /// `sync_hub_pg.unsupported_context` when the thread is inside a tokio runtime, the bridge
    /// allows more calls in flight than the pool can serve, or the tenant is empty or too long.
    pub fn new(
        bridge: Arc<Bridge>,
        pool_size: usize,
        tenant: impl Into<String>,
    ) -> Result<Self, PgHubError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(PgHubError::UnsupportedContext {
                reason: "the store is built inside an async runtime; build it on a plain thread",
            });
        }
        if bridge.config().max_in_flight > BridgeConfig::largest_in_flight(pool_size) {
            return Err(PgHubError::UnsupportedContext {
                reason: "the bridge allows more calls in flight than the pool can serve",
            });
        }
        let tenant = tenant.into();
        if tenant.is_empty() || tenant.len() > MAX_NAME || tenant.chars().any(char::is_control) {
            return Err(PgHubError::UnsupportedContext {
                reason: "the tenant is empty, too long or not printable",
            });
        }
        Ok(Self { bridge, tenant })
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates every table (idempotent).
    ///
    /// # Errors
    /// `sync_hub_pg.prepare_failed` when the DDL cannot be applied or the bridge refuses.
    pub fn prepare(&self) -> Result<(), PgHubError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn| {
                Box::pin(async move { schema::prepare(&mut conn).await })
            })
            .map_err(|_| PgHubError::PrepareFailed)
    }

    /// The hub tables that do not exist. The host refuses to serve while this is not empty.
    ///
    /// # Errors
    /// The store's failure; a database that cannot answer is not "no tables missing".
    pub fn missing_tables(&self) -> Result<Vec<&'static str>, PgHubError> {
        self.bridge
            .run(PREPARE_DEADLINE, |conn| {
                Box::pin(async move { schema::missing(&conn).await })
            })
            .map_err(map)
    }

    /// Fails with `sync_hub_pg.tables_missing` when any table is missing.
    ///
    /// # Errors
    /// `sync_hub_pg.tables_missing`, or the store's failure.
    pub fn require_tables(&self) -> Result<(), PgHubError> {
        let tables = self.missing_tables()?;
        if tables.is_empty() {
            Ok(())
        } else {
            Err(PgHubError::TablesMissing { tables })
        }
    }

    /// Runs `op` on a pooled connection with `deadline`.
    pub(crate) fn run<T, F>(
        &self,
        deadline: Duration,
        op: F,
    ) -> Result<T, spot_pg_bridge::BridgeError<OpError>>
    where
        T: Send + 'static,
        F: FnOnce(
                spot_pg_bridge::PooledConn,
            ) -> spot_pg_bridge::BoxFuture<'static, Result<T, OpError>>
            + Send
            + 'static,
    {
        self.bridge.run(deadline, op)
    }
}

/// Every store of one tenant, built together.
#[derive(Clone, Debug)]
pub struct PgHub {
    db: Db,
}

impl PgHub {
    /// A hub handle; see [`Db::new`].
    ///
    /// # Errors
    /// As [`Db::new`].
    pub fn new(
        bridge: Arc<Bridge>,
        pool_size: usize,
        tenant: impl Into<String>,
    ) -> Result<Self, PgHubError> {
        Ok(Self {
            db: Db::new(bridge, pool_size, tenant)?,
        })
    }

    /// The shared handle.
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Creates the tables.
    ///
    /// # Errors
    /// As [`Db::prepare`].
    pub fn prepare(&self) -> Result<(), PgHubError> {
        self.db.prepare()
    }

    /// The send queue with `limits`.
    #[must_use]
    pub fn queue(&self, limits: QueueLimits) -> PgSendQueue {
        PgSendQueue::new(self.db.clone(), limits)
    }

    /// The relay state store.
    #[must_use]
    pub fn relay_state(&self) -> PgRelayStore {
        PgRelayStore::new(self.db.clone())
    }

    /// The node registry.
    #[must_use]
    pub fn nodes(&self) -> PgNodeRegistry {
        PgNodeRegistry::new(self.db.clone())
    }

    /// The acknowledgement book.
    #[must_use]
    pub fn acks(&self) -> PgAckBook {
        PgAckBook::new(self.db.clone())
    }

    /// The obligation ledger's store.
    #[must_use]
    pub fn ledger_store(&self) -> PgNodeStateStore {
        PgNodeStateStore::new(self.db.clone())
    }
}
