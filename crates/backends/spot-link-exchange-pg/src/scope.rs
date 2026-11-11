//! The connection scope shared by the three stores: a pool, a runtime to block on, and the
//! tenant's schema.
//!
//! A [`PgExchange`] is built for one [`TenantId`]. Every transaction starts by making that
//! tenant's schema (`t_<id>`, derived from the id and never from a tenant-chosen string) the only
//! entry of `search_path`, transaction-locally, so the statements, all of them parameterised and
//! none built from caller text, can see no other tenant's rows.

use std::sync::Arc;

use deadpool_postgres::{Object, Pool, Transaction};
use spot_app_api::AppResult;
use spot_link_exchange::Result;
use spot_tenancy::TenantId;

use crate::blocking::block_on;
use crate::error::{q, store_failed};
use crate::feed::PgNoticeFeed;
use crate::inbox::PgInbox;
use crate::outbox::PgOutbox;
use crate::schema::prepare_tenant;

#[derive(Clone)]
pub(crate) struct Scope {
    pub(crate) pool: Pool,
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
    pub(crate) schema: String,
}

impl Scope {
    pub(crate) async fn conn(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|_| store_failed("no exchange store connection is available"))
    }

    /// Begins a read-write transaction scoped to the tenant's schema.
    pub(crate) async fn begin<'c>(&self, conn: &'c mut Object) -> Result<Transaction<'c>> {
        let tx = conn
            .transaction()
            .await
            .map_err(|_| store_failed("the exchange store transaction could not begin"))?;
        tx.query_one(
            "SELECT set_config('search_path', $1, true)",
            &[&self.schema],
        )
        .await
        .map_err(q)?;
        Ok(tx)
    }

    pub(crate) fn run<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        block_on(&self.runtime, future)
    }
}

/// The exchange stores of one tenant.
#[derive(Clone)]
pub struct PgExchange {
    scope: Scope,
    tenant: TenantId,
}

impl std::fmt::Debug for PgExchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgExchange")
            .field("tenant", &self.tenant.to_string())
            .finish_non_exhaustive()
    }
}

impl PgExchange {
    /// The stores of `tenant` on `pool`, the pool of the database that holds the tenant's
    /// schema. `runtime` is what the synchronous trait methods block on; do not call them from a
    /// current-thread runtime.
    #[must_use]
    pub fn new(pool: Pool, runtime: Arc<tokio::runtime::Runtime>, tenant: TenantId) -> Self {
        let schema = tenant.schema_name();
        Self {
            scope: Scope {
                pool,
                runtime,
                schema,
            },
            tenant,
        }
    }

    /// Creates the exchange tables in the tenant's schema if they are missing. Idempotent and
    /// safe from several nodes at once. The tenant's schema must already exist.
    ///
    /// # Errors
    /// `link_exchange_pg.migration_failed`.
    pub async fn prepare(&self) -> AppResult<()> {
        prepare_tenant(&self.scope.pool, &self.scope.schema).await
    }

    /// The tenant these stores belong to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The sender's store.
    #[must_use]
    pub fn outbox(&self) -> PgOutbox {
        PgOutbox::new(self.scope.clone())
    }

    /// The receiver's store.
    #[must_use]
    pub fn inbox(&self) -> PgInbox {
        PgInbox::new(self.scope.clone())
    }

    /// The delivery notice feed.
    #[must_use]
    pub fn notices(&self) -> PgNoticeFeed {
        PgNoticeFeed::new(self.scope.clone())
    }
}
