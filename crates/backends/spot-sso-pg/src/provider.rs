//! [`PgProviderStore`]: the tenant-scoped PostgreSQL [`ProviderStore`], over `spot_sso_provider`
//! (WP-IAM-05.T9).
//!
//! | Column | Holds |
//! | --- | --- |
//! | `tenant` | the tenant the store serves; in every predicate of every statement |
//! | `id` | the provider id |
//! | `audience` | `staff`, `partner` or `consumer`, stored with the row (and checked against the configuration on read) |
//! | `config` | the [`ProviderConfig`] as JSON: the client secret is only the **name** of a `spot-secrets` entry |
//!
//! Every write and every read goes through [`ProviderConfig::validate`]: an invalid configuration
//! is refused on write; a stored row that no longer validates (or whose audience column disagrees
//! with its configuration, or that cannot be parsed) is **not served**: `list` leaves it out and
//! names it in `skipped`, `get` reports it as [`ProviderStoreError::Invalid`]. A database failure
//! is [`ProviderStoreError::Unavailable`] with no driver text.

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_sso::provider_store::check;
use spot_sso::{
    ProviderConfig, ProviderId, ProviderListing, ProviderResult, ProviderStore, ProviderStoreError,
    SsoError, SsoErrors, StoreFuture, codes,
};

use crate::error::{migration, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5353_4f50_0003;
/// The most bytes a stored configuration may take (the schema enforces it too).
const MAX_CONFIG: usize = 65_536;

/// The idempotent DDL of the provider table. Every statement can run again.
pub const PROVIDER_DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_sso_provider (
    tenant   text NOT NULL,
    id       text NOT NULL,
    audience text NOT NULL,
    config   text NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_sso_provider_keys CHECK (
        octet_length(tenant) BETWEEN 1 AND 128
        AND octet_length(id) BETWEEN 1 AND 128
        AND audience IN ('staff', 'partner', 'consumer')
        AND octet_length(config) BETWEEN 2 AND 65536)
);
";

const LIST: &str = "SELECT id, audience, config FROM spot_sso_provider \
    WHERE tenant = $1 ORDER BY id COLLATE \"C\"";
const GET: &str =
    "SELECT id, audience, config FROM spot_sso_provider WHERE tenant = $1 AND id = $2";
const PUT: &str = "INSERT INTO spot_sso_provider (tenant, id, audience, config) \
    VALUES ($1, $2, $3, $4) \
    ON CONFLICT (tenant, id) DO UPDATE SET audience = EXCLUDED.audience, config = EXCLUDED.config";
const DELETE: &str = "DELETE FROM spot_sso_provider WHERE tenant = $1 AND id = $2 RETURNING 1";

/// The provider-configuration store in PostgreSQL, for one tenant.
#[derive(Clone)]
pub struct PgProviderStore {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgProviderStore {
    /// Names the tenant, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgProviderStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

/// A stored row that cannot be served.
fn invalid(detail: &'static str) -> ProviderStoreError {
    ProviderStoreError::Invalid(SsoErrors::one(
        SsoError::new(
            codes::PROVIDER_INVALID,
            "The identity provider settings are not valid.",
            "Correct the provider configuration so that it passes validation, then save it again.",
        )
        .with_detail(detail),
    ))
}

/// Parses and validates one stored row.
fn serve(id: &str, audience: &str, config: &str) -> ProviderResult<ProviderConfig> {
    let parsed: ProviderConfig =
        serde_json::from_str(config).map_err(|_| invalid("the stored row cannot be read"))?;
    if parsed.id.as_str() != id || parsed.audience.as_str() != audience {
        return Err(invalid("the stored row disagrees with its key or audience"));
    }
    check(&parsed)?;
    Ok(parsed)
}

impl PgProviderStore {
    /// A store on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
    #[must_use]
    pub fn new(pool: Pool, tenant: impl Into<String>) -> Option<Self> {
        let tenant = tenant.into();
        if tenant.is_empty() || tenant.len() > 128 {
            return None;
        }
        Some(Self { pool, tenant })
    }

    /// The tenant this store serves.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the table if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `sso_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        let mut conn = self.pool.get().await.map_err(|e| migration_pool(&e))?;
        let tx = conn.transaction().await.map_err(|e| migration(&e))?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
            .await
            .map_err(|e| migration(&e))?;
        tx.batch_execute(PROVIDER_DDL)
            .await
            .map_err(|e| migration(&e))?;
        tx.commit().await.map_err(|e| migration(&e))
    }

    async fn conn(&self) -> ProviderResult<Object> {
        self.pool
            .get()
            .await
            .map_err(|_| ProviderStoreError::Unavailable)
    }

    async fn list_all(&self) -> ProviderResult<ProviderListing> {
        let conn = self.conn().await?;
        let rows = conn
            .query(LIST, &[&self.tenant])
            .await
            .map_err(|_| ProviderStoreError::Unavailable)?;
        let mut listing = ProviderListing::default();
        for row in rows {
            let id: String = row
                .try_get(0)
                .map_err(|_| ProviderStoreError::Unavailable)?;
            let audience: String = row
                .try_get(1)
                .map_err(|_| ProviderStoreError::Unavailable)?;
            let config: String = row
                .try_get(2)
                .map_err(|_| ProviderStoreError::Unavailable)?;
            match serve(&id, &audience, &config) {
                Ok(parsed) => listing.providers.push(parsed),
                Err(_) => listing.skipped.push(ProviderId::new(id)),
            }
        }
        Ok(listing)
    }

    async fn get_one(&self, id: &ProviderId) -> ProviderResult<Option<ProviderConfig>> {
        let conn = self.conn().await?;
        let row = conn
            .query_opt(GET, &[&self.tenant, &id.as_str()])
            .await
            .map_err(|_| ProviderStoreError::Unavailable)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row
            .try_get(0)
            .map_err(|_| ProviderStoreError::Unavailable)?;
        let audience: String = row
            .try_get(1)
            .map_err(|_| ProviderStoreError::Unavailable)?;
        let config: String = row
            .try_get(2)
            .map_err(|_| ProviderStoreError::Unavailable)?;
        serve(&id, &audience, &config).map(Some)
    }

    async fn put_one(&self, config: &ProviderConfig) -> ProviderResult<()> {
        check(config)?;
        let text = serde_json::to_string(config).map_err(|_| invalid("not serialisable"))?;
        if text.len() > MAX_CONFIG {
            return Err(invalid("the configuration is too large"));
        }
        let audience = config.audience.as_str();
        let conn = self.conn().await?;
        conn.execute(PUT, &[&self.tenant, &config.id.as_str(), &audience, &text])
            .await
            .map(|_| ())
            .map_err(|_| ProviderStoreError::Unavailable)
    }

    async fn delete_one(&self, id: &ProviderId) -> ProviderResult<bool> {
        let conn = self.conn().await?;
        conn.query_opt(DELETE, &[&self.tenant, &id.as_str()])
            .await
            .map(|row| row.is_some())
            .map_err(|_| ProviderStoreError::Unavailable)
    }
}

impl ProviderStore for PgProviderStore {
    fn list(&self) -> StoreFuture<'_, ProviderResult<ProviderListing>> {
        Box::pin(self.list_all())
    }

    fn get<'a>(
        &'a self,
        id: &'a ProviderId,
    ) -> StoreFuture<'a, ProviderResult<Option<ProviderConfig>>> {
        Box::pin(self.get_one(id))
    }

    fn put<'a>(&'a self, config: &'a ProviderConfig) -> StoreFuture<'a, ProviderResult<()>> {
        Box::pin(self.put_one(config))
    }

    fn delete<'a>(&'a self, id: &'a ProviderId) -> StoreFuture<'a, ProviderResult<bool>> {
        Box::pin(self.delete_one(id))
    }
}
