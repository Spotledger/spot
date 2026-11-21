//! The async store over the caller's pool.

use std::fmt;

use deadpool_postgres::Pool;

use crate::error::NodeRoleStoreError;

/// What the table holds for a site: the role as text (the caller validates it) and the pin.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredRole {
    /// The recorded role, as stored.
    pub role: String,
    /// The pinned hub fingerprint, if any.
    pub hub_fingerprint: Option<String>,
}

impl fmt::Debug for StoredRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredRole")
            .field("role", &self.role)
            .field(
                "hub_fingerprint",
                &self.hub_fingerprint.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The PostgreSQL node role store.
#[derive(Clone)]
pub struct PgNodeRoleStore {
    pool: Pool,
}

impl fmt::Debug for PgNodeRoleStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgNodeRoleStore").finish_non_exhaustive()
    }
}

impl PgNodeRoleStore {
    /// Builds the store over `pool`. No I/O.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// The recorded role of `site`, or `None` when it has no row. Any failure is an error.
    ///
    /// # Errors
    /// `node_role_pg.table_missing` or `node_role_pg.unavailable`.
    pub async fn read(&self, site: &str) -> Result<Option<StoredRole>, NodeRoleStoreError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|_| NodeRoleStoreError::unavailable())?;
        let row = client
            .query_opt(
                "SELECT role, hub_fingerprint FROM spot_node_role_state WHERE site = $1",
                &[&site],
            )
            .await
            .map_err(|e| NodeRoleStoreError::from_driver(&e))?;
        match row {
            None => Ok(None),
            Some(row) => Ok(Some(StoredRole {
                role: row
                    .try_get(0)
                    .map_err(|_| NodeRoleStoreError::unavailable())?,
                hub_fingerprint: row
                    .try_get(1)
                    .map_err(|_| NodeRoleStoreError::unavailable())?,
            })),
        }
    }

    /// Records `role` for `site`. A `None` fingerprint keeps the stored one. The store does not
    /// decide whether the change is allowed.
    ///
    /// # Errors
    /// `node_role_pg.table_missing` or `node_role_pg.unavailable`.
    pub async fn upsert(
        &self,
        site: &str,
        role: &str,
        hub_fingerprint: Option<&str>,
        updated_at_unix: i64,
    ) -> Result<(), NodeRoleStoreError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|_| NodeRoleStoreError::unavailable())?;
        client
            .execute(
                "INSERT INTO spot_node_role_state (site, role, hub_fingerprint, updated_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (site) DO UPDATE SET role = EXCLUDED.role, \
                 hub_fingerprint = COALESCE(EXCLUDED.hub_fingerprint, spot_node_role_state.hub_fingerprint), \
                 updated_at = EXCLUDED.updated_at",
                &[&site, &role, &hub_fingerprint, &updated_at_unix],
            )
            .await
            .map_err(|e| NodeRoleStoreError::from_driver(&e))?;
        Ok(())
    }
}

/// Test helper: drops the table.
///
/// # Errors
/// `node_role_pg.unavailable`.
#[cfg(feature = "testing")]
pub async fn drop_table_for_tests(pool: &Pool) -> Result<(), NodeRoleStoreError> {
    let client = pool
        .get()
        .await
        .map_err(|_| NodeRoleStoreError::unavailable())?;
    client
        .batch_execute("DROP TABLE IF EXISTS spot_node_role_state")
        .await
        .map_err(|e| NodeRoleStoreError::from_driver(&e))
}

/// Test helper: creates the table from [`schema::DDL`](crate::schema::DDL).
///
/// # Errors
/// `node_role_pg.unavailable`.
#[cfg(feature = "testing")]
pub async fn create_table_for_tests(pool: &Pool) -> Result<(), NodeRoleStoreError> {
    let client = pool
        .get()
        .await
        .map_err(|_| NodeRoleStoreError::unavailable())?;
    client
        .batch_execute(crate::schema::DDL)
        .await
        .map_err(|e| NodeRoleStoreError::from_driver(&e))
}
