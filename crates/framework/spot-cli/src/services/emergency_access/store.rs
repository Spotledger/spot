//! Where emergency grants are kept: one text value per grant under `grant:<id>`, written with a
//! compare-and-swap so two nodes (or a node and a command) cannot both change one grant.
//!
//! The key-value shape is the one the governance module already uses ([`GovernanceStore`]), so
//! the in-memory store of its tests serves here too. The production store is one table,
//! `spot_emergency_access (tenant, key, value)`, created at start with `CREATE TABLE IF NOT
//! EXISTS` (the convention of the trigger marks and the other host tables, `spot_` prefix and the
//! tenant in the key). Nothing in the table is a secret: a grant's reason and its approver and
//! controller names, as the state machine holds them.

use std::sync::Arc;

use deadpool_postgres::Pool;
use spot_http::api::service::BoxFuture;

use super::model::{Body, CODE_NOT_FOUND, CODE_UNAVAILABLE, EmergencyError, Stored};
use crate::services::governance::{GovernanceStore, StoreFailure, StoreResult};

const PREFIX: &str = "grant:";

/// The key of grant `id`.
#[must_use]
pub fn key_of(id: &str) -> String {
    format!("{PREFIX}{id}")
}

/// The grants of one site over a key-value store.
#[derive(Clone)]
pub struct GrantStore {
    kv: Arc<dyn GovernanceStore>,
    site: String,
}

/// A loaded grant with the exact text it was read as (the compare-and-swap's expectation).
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The stored text.
    pub raw: String,
    /// The decoded record.
    pub body: Body,
}

impl GrantStore {
    /// The store of `site` over `kv`.
    #[must_use]
    pub fn new(kv: Arc<dyn GovernanceStore>, site: &str) -> Self {
        Self {
            kv,
            site: site.to_owned(),
        }
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// Loads grant `id`.
    ///
    /// # Errors
    ///
    /// `not_found` when there is none; `unavailable` when the store fails; `state_invalid` or
    /// `state_version` when the record is not one this release reads (never repaired).
    pub async fn load(&self, id: &str) -> Result<Loaded, EmergencyError> {
        let raw = self
            .kv
            .get(&key_of(id))
            .await
            .map_err(|_| EmergencyError::new(CODE_UNAVAILABLE))?
            .ok_or(EmergencyError::new(CODE_NOT_FOUND))?;
        let body = Stored::decode(&raw, &self.site, id)?;
        Ok(Loaded { raw, body })
    }

    /// Writes `body` when the stored text is still `expected` (`None`: a new grant). `Ok(false)`
    /// when another writer got there first; nothing was written then.
    ///
    /// # Errors
    ///
    /// `unavailable` when the store fails or the body cannot be encoded.
    pub async fn put(&self, expected: Option<&str>, body: &Body) -> Result<bool, EmergencyError> {
        let text = Stored::encode(body)?;
        self.kv
            .compare_put(&key_of(&body.id), expected, &text)
            .await
            .map_err(|_| EmergencyError::new(CODE_UNAVAILABLE))
    }

    /// Every grant, in id order. A record that does not decode is reported as an error for that
    /// record, not skipped: the caller says which.
    ///
    /// # Errors
    ///
    /// `unavailable` when the listing fails.
    pub async fn all(&self) -> Result<Vec<Result<Loaded, EmergencyError>>, EmergencyError> {
        let texts = self
            .kv
            .list(PREFIX)
            .await
            .map_err(|_| EmergencyError::new(CODE_UNAVAILABLE))?;
        Ok(texts
            .into_iter()
            .map(|raw| {
                // The id is inside the record; the key is checked against it by `decode`.
                let id = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("body")?.get("id")?.as_str().map(str::to_owned))
                    .ok_or(EmergencyError::new(super::model::CODE_STATE_INVALID))?;
                let body = Stored::decode(&raw, &self.site, &id)?;
                Ok(Loaded { raw, body })
            })
            .collect())
    }
}

/// The durable store: one table in the site's database.
pub struct PgKv {
    pool: Pool,
    tenant: String,
}

impl PgKv {
    /// A store of `tenant` over `pool`.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Self {
        Self {
            pool,
            tenant: tenant.to_owned(),
        }
    }

    /// Creates the table; safe to repeat.
    ///
    /// # Errors
    ///
    /// [`StoreFailure`] when the database cannot be reached or refuses it.
    pub async fn prepare(&self) -> StoreResult<()> {
        let client = self.pool.get().await.map_err(|_| StoreFailure)?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS spot_emergency_access (\
                 tenant text NOT NULL, key text NOT NULL, value text NOT NULL, \
                 PRIMARY KEY (tenant, key))",
            )
            .await
            .map_err(|_| StoreFailure)
    }
}

impl GovernanceStore for PgKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| StoreFailure)?;
            let row = client
                .query_opt(
                    "SELECT value FROM spot_emergency_access WHERE tenant = $1 AND key = $2",
                    &[&self.tenant, &key],
                )
                .await
                .map_err(|_| StoreFailure)?;
            Ok(row.map(|r| r.get::<_, String>(0)))
        })
    }

    fn compare_put<'a>(
        &'a self,
        key: &'a str,
        expected: Option<&'a str>,
        new: &'a str,
    ) -> BoxFuture<'a, StoreResult<bool>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| StoreFailure)?;
            let changed = match expected {
                None => client
                    .execute(
                        "INSERT INTO spot_emergency_access (tenant, key, value) \
                         VALUES ($1, $2, $3) ON CONFLICT (tenant, key) DO NOTHING",
                        &[&self.tenant, &key, &new],
                    )
                    .await
                    .map_err(|_| StoreFailure)?,
                Some(old) => client
                    .execute(
                        "UPDATE spot_emergency_access SET value = $3 \
                         WHERE tenant = $1 AND key = $2 AND value = $4",
                        &[&self.tenant, &key, &new, &old],
                    )
                    .await
                    .map_err(|_| StoreFailure)?,
            };
            Ok(changed == 1)
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, StoreResult<Vec<String>>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| StoreFailure)?;
            let rows = client
                .query(
                    "SELECT value FROM spot_emergency_access \
                     WHERE tenant = $1 AND left(key, char_length($2::text)) = $2::text \
                     ORDER BY key",
                    &[&self.tenant, &prefix],
                )
                .await
                .map_err(|_| StoreFailure)?;
            Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
        })
    }
}
