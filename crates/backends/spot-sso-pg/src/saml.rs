//! The SAML provider rows (WP-IAM-05.T18): [`PgSamlRows`] is the SQL of `spot_sso_saml_provider`,
//! moved here from the host so the G5 lint (no SQL and no database driver outside the storage
//! crates) passes.
//!
//! The table has the shape of the OpenID Connect provider table plus a version:
//!
//! | Column | Holds |
//! | --- | --- |
//! | `tenant` | the tenant the store serves; in every predicate of every statement |
//! | `id` | the provider id |
//! | `audience` | `staff` or `partner`, stored with the row |
//! | `version` | starts at 1 and moves by one on every change: the compare-and-set guard of every update and delete |
//! | `config` | the provider as JSON text; this crate never reads inside it (the host owns its shape: certificates in hex, the signing key only as the *name* of a secret) |
//!
//! Every statement names the tenant, so another tenant's row is exactly as missing. A database
//! failure is [`SamlRowError::Unavailable`] with no driver text. Whether a row is trustworthy (does
//! it parse, validate, agree with its key and audience) is the host's decision, made on the row this
//! store returns.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::sql::run_ddl;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5353_4f50_0101;

/// The most bytes a stored configuration may take (the schema enforces it too).
pub const MAX_SAML_CONFIG: usize = 131_072;

/// The idempotent DDL of the table. Every statement can run again.
pub const SAML_DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_sso_saml_provider (
    tenant   text   NOT NULL,
    id       text   NOT NULL,
    audience text   NOT NULL,
    version  bigint NOT NULL,
    config   text   NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_sso_saml_provider_keys CHECK (
        octet_length(tenant) BETWEEN 1 AND 128
        AND octet_length(id) BETWEEN 1 AND 128
        AND audience IN ('staff', 'partner')
        AND version >= 1
        AND octet_length(config) BETWEEN 2 AND 131072)
);
";

const LIST: &str = "SELECT id, version, audience, config FROM spot_sso_saml_provider \
    WHERE tenant = $1 ORDER BY id COLLATE \"C\"";
const GET: &str = "SELECT version, audience, config FROM spot_sso_saml_provider \
    WHERE tenant = $1 AND id = $2";
const INSERT: &str = "INSERT INTO spot_sso_saml_provider (tenant, id, audience, version, config) \
    VALUES ($1, $2, $3, 1, $4) ON CONFLICT (tenant, id) DO NOTHING RETURNING version";
const UPDATE: &str = "UPDATE spot_sso_saml_provider \
    SET audience = $3, version = version + 1, config = $4 \
    WHERE tenant = $1 AND id = $2 AND version = $5 RETURNING version";
const DELETE: &str = "DELETE FROM spot_sso_saml_provider \
    WHERE tenant = $1 AND id = $2 AND version = $3 RETURNING version";

/// Why a row call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamlRowError {
    /// The store could not answer; nothing was changed.
    Unavailable,
    /// An insert found the provider already there.
    Exists,
    /// An update or delete named a version that is no longer the stored one, or no such provider.
    Stale,
}

/// One stored row, as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamlRow {
    /// The provider id.
    pub id: String,
    /// The stored version.
    pub version: i64,
    /// The audience column (`staff` or `partner`).
    pub audience: String,
    /// The configuration, as stored.
    pub config: String,
}

/// The SAML provider rows of one tenant.
#[derive(Clone)]
pub struct PgSamlRows {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgSamlRows {
    /// Names the tenant, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSamlRows")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgSamlRows {
    /// Rows on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Option<Self> {
        (!tenant.is_empty() && tenant.len() <= 128).then(|| Self {
            pool,
            tenant: tenant.to_owned(),
        })
    }

    /// The tenant this store serves.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the table if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `sso_pg.migration_failed`.
    pub async fn prepare(&self) -> AppResult<()> {
        run_ddl(&self.pool, PREPARE_LOCK, SAML_DDL).await
    }

    /// Every row of the tenant, ordered by id.
    ///
    /// # Errors
    /// [`SamlRowError::Unavailable`].
    pub async fn list(&self) -> Result<Vec<SamlRow>, SamlRowError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        let rows = conn
            .query(LIST, &[&self.tenant])
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        rows.iter()
            .map(|row| {
                Ok(SamlRow {
                    id: row.try_get(0).map_err(|_| SamlRowError::Unavailable)?,
                    version: row.try_get(1).map_err(|_| SamlRowError::Unavailable)?,
                    audience: row.try_get(2).map_err(|_| SamlRowError::Unavailable)?,
                    config: row.try_get(3).map_err(|_| SamlRowError::Unavailable)?,
                })
            })
            .collect()
    }

    /// The row of `id`; `None` when the tenant has none.
    ///
    /// # Errors
    /// [`SamlRowError::Unavailable`].
    pub async fn get(&self, id: &str) -> Result<Option<SamlRow>, SamlRowError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        let row = conn
            .query_opt(GET, &[&self.tenant, &id])
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        row.map(|row| {
            Ok(SamlRow {
                id: id.to_owned(),
                version: row.try_get(0).map_err(|_| SamlRowError::Unavailable)?,
                audience: row.try_get(1).map_err(|_| SamlRowError::Unavailable)?,
                config: row.try_get(2).map_err(|_| SamlRowError::Unavailable)?,
            })
        })
        .transpose()
    }

    /// Creates the row at version 1 and returns the version.
    ///
    /// # Errors
    /// [`SamlRowError::Exists`] when the id is taken, [`SamlRowError::Unavailable`].
    pub async fn insert(
        &self,
        id: &str,
        audience: &str,
        config: &str,
    ) -> Result<i64, SamlRowError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        let row = conn
            .query_opt(INSERT, &[&self.tenant, &id, &audience, &config])
            .await
            .map_err(|_| SamlRowError::Unavailable)?
            .ok_or(SamlRowError::Exists)?;
        row.try_get(0).map_err(|_| SamlRowError::Unavailable)
    }

    /// Replaces the row if its version is `expected`, and returns the new version.
    ///
    /// # Errors
    /// [`SamlRowError::Stale`] (a different version, or none), [`SamlRowError::Unavailable`].
    pub async fn update(
        &self,
        id: &str,
        audience: &str,
        config: &str,
        expected: i64,
    ) -> Result<i64, SamlRowError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        let row = conn
            .query_opt(UPDATE, &[&self.tenant, &id, &audience, &config, &expected])
            .await
            .map_err(|_| SamlRowError::Unavailable)?
            .ok_or(SamlRowError::Stale)?;
        row.try_get(0).map_err(|_| SamlRowError::Unavailable)
    }

    /// Deletes the row if its version is `expected`.
    ///
    /// # Errors
    /// [`SamlRowError::Stale`] (a different version, or none), [`SamlRowError::Unavailable`].
    pub async fn delete(&self, id: &str, expected: i64) -> Result<(), SamlRowError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| SamlRowError::Unavailable)?;
        conn.query_opt(DELETE, &[&self.tenant, &id, &expected])
            .await
            .map_err(|_| SamlRowError::Unavailable)?
            .map(|_| ())
            .ok_or(SamlRowError::Stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_names_the_tenant() {
        for sql in [LIST, GET, INSERT, UPDATE, DELETE] {
            assert!(
                sql.contains("tenant = $1") || sql.contains("(tenant,"),
                "{sql}"
            );
        }
    }
}
