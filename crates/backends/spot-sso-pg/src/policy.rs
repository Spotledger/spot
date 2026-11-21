//! The tenant sign-in policy row (WP-IAM-05.T18): [`PgPolicyStore`] is the SQL of `spot_sso_policy`,
//! moved here from the host so the G5 lint passes. The host adapts it to the password door's
//! `PolicySource` port and to the administration API.
//!
//! One row per tenant (`tenant`, `version`, `policy` as JSON of [`SignInPolicy`]). A tenant with
//! **no row** has never configured single sign-on: its policy is `local`, version 0. A row that
//! cannot be read or parsed, or a database that does not answer, is **not** `local`: it is
//! [`PolicyStoreError::Unavailable`] and the caller refuses (fail closed). Changes are
//! compare-and-set on the version, so two administrators changing the policy at once cannot both
//! win and the loser is told ([`PolicyStoreError::Stale`]).

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use spot_sso::SignInPolicy;

use crate::sql::run_ddl;

const PREPARE_LOCK: i64 = 0x5353_4f50_0102;
const MAX_POLICY: usize = 65_536;

/// The idempotent DDL of the policy table.
pub const POLICY_DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_sso_policy (
    tenant  text   NOT NULL,
    version bigint NOT NULL,
    policy  text   NOT NULL,
    PRIMARY KEY (tenant),
    CONSTRAINT spot_sso_policy_keys CHECK (
        octet_length(tenant) BETWEEN 1 AND 128
        AND version >= 1
        AND octet_length(policy) BETWEEN 2 AND 65536)
);
";

const GET: &str = "SELECT version, policy FROM spot_sso_policy WHERE tenant = $1";
const INSERT: &str = "INSERT INTO spot_sso_policy (tenant, version, policy) VALUES ($1, 1, $2) \
    ON CONFLICT (tenant) DO NOTHING RETURNING version";
const UPDATE: &str = "UPDATE spot_sso_policy SET version = version + 1, policy = $2 \
    WHERE tenant = $1 AND version = $3 RETURNING version";

/// Why a policy call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyStoreError {
    /// The store could not answer, or the stored policy cannot be read; nothing changed.
    Unavailable,
    /// The write named a version that is no longer the stored one.
    Stale,
}

/// The policy store of one tenant.
#[derive(Clone)]
pub struct PgPolicyStore {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgPolicyStore {
    /// Names the tenant, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgPolicyStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgPolicyStore {
    /// A store on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
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
        run_ddl(&self.pool, PREPARE_LOCK, POLICY_DDL).await
    }

    /// The stored policy and its version; `(local, 0)` for a tenant with no row.
    ///
    /// # Errors
    /// [`PolicyStoreError::Unavailable`] when the database does not answer or the row does not
    /// parse (never `local` by mistake).
    pub async fn read(&self) -> Result<(SignInPolicy, i64), PolicyStoreError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| PolicyStoreError::Unavailable)?;
        let row = conn
            .query_opt(GET, &[&self.tenant])
            .await
            .map_err(|_| PolicyStoreError::Unavailable)?;
        let Some(row) = row else {
            return Ok((SignInPolicy::local(), 0));
        };
        let text: String = row.try_get(1).map_err(|_| PolicyStoreError::Unavailable)?;
        let policy: SignInPolicy =
            serde_json::from_str(&text).map_err(|_| PolicyStoreError::Unavailable)?;
        let version: i64 = row.try_get(0).map_err(|_| PolicyStoreError::Unavailable)?;
        Ok((policy, version))
    }

    /// Replaces the policy if the stored version is `expected` (0: no row yet). Returns the new
    /// version.
    ///
    /// # Errors
    /// [`PolicyStoreError::Stale`], [`PolicyStoreError::Unavailable`].
    pub async fn write(
        &self,
        policy: &SignInPolicy,
        expected: i64,
    ) -> Result<i64, PolicyStoreError> {
        let text = serde_json::to_string(policy).map_err(|_| PolicyStoreError::Unavailable)?;
        if text.len() > MAX_POLICY {
            return Err(PolicyStoreError::Unavailable);
        }
        let conn = self
            .pool
            .get()
            .await
            .map_err(|_| PolicyStoreError::Unavailable)?;
        let row = if expected == 0 {
            conn.query_opt(INSERT, &[&self.tenant, &text]).await
        } else {
            conn.query_opt(UPDATE, &[&self.tenant, &text, &expected])
                .await
        }
        .map_err(|_| PolicyStoreError::Unavailable)?;
        row.ok_or(PolicyStoreError::Stale)?
            .try_get(0)
            .map_err(|_| PolicyStoreError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_names_the_tenant() {
        for sql in [GET, INSERT, UPDATE] {
            assert!(
                sql.contains("tenant = $1") || sql.contains("(tenant,"),
                "{sql}"
            );
        }
    }
}
