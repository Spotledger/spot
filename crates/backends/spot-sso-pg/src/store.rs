//! [`PgReplayStore`]: the PostgreSQL [`ReplayStore`], over `spot_sso_replay`.

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_sso::{Claim, ReplayError, ReplayStore, StoreFuture};

use crate::error::{pool_unavailable, unavailable};
use crate::sql;

/// The replay store in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgReplayStore::prepare`] once at
/// start-up; clones share the pool, and a restart is a new store over the same database and
/// namespace, so it sees every claim the previous process made.
#[derive(Clone)]
pub struct PgReplayStore {
    pool: Pool,
    namespace: String,
}

impl std::fmt::Debug for PgReplayStore {
    /// Names the namespace, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgReplayStore")
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl PgReplayStore {
    /// A store on `pool`, the tenant's own pool, claiming in `namespace`. It does not touch the
    /// database until used.
    ///
    /// The namespace is the scope of the keys: every row is keyed `(namespace, id)`, so one table
    /// can serve several stores (several tenants or several deployments) without a shared id ever
    /// colliding across them. It is part of the identity of a claim — a store that changes its
    /// namespace no longer sees the ids the old one recorded.
    #[must_use]
    pub fn new(pool: Pool, namespace: impl Into<String>) -> Self {
        Self {
            pool,
            namespace: namespace.into(),
        }
    }

    /// The namespace this store claims in.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Creates the table and its indexes if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `sso_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        crate::schema::prepare(&self.pool).await
    }

    /// Deletes at most `limit` rows whose `expires_at` is at or before `now`, and returns how many
    /// were deleted.
    ///
    /// Bounded on purpose: a cleanup job calls it in a loop, so no single statement locks an
    /// unbounded number of rows. It is table-wide rather than namespace-wide, and that is safe:
    /// an expired id is free to claim again, so removing one — whichever namespace recorded it —
    /// takes nothing a caller could still rely on. A live row is never touched.
    ///
    /// # Errors
    /// [`ReplayError`] when the database cannot answer. A caller that is only tidying up may log
    /// the failure and try again later; it must not read it as "pruned".
    pub async fn prune(&self, now: i64, limit: u32) -> Result<u64, ReplayError> {
        let conn = self.conn().await?;
        let limit = i64::from(limit);
        conn.execute(sql::PRUNE, &[&now, &limit])
            .await
            .map_err(|e| unavailable(&e))
    }

    async fn conn(&self) -> Result<Object, ReplayError> {
        self.pool.get().await.map_err(|e| pool_unavailable(&e))
    }
}

impl ReplayStore for PgReplayStore {
    /// One statement decides: a returned row is [`Claim::Fresh`], no row is [`Claim::Replayed`].
    fn claim<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        expires_at: i64,
    ) -> StoreFuture<'a, Result<Claim, ReplayError>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            let row = conn
                .query_opt(sql::CLAIM, &[&self.namespace, &id, &expires_at, &now])
                .await
                .map_err(|e| unavailable(&e))?;
            Ok(if row.is_some() {
                Claim::Fresh
            } else {
                Claim::Replayed
            })
        })
    }
}
