//! [`PgCursorStore`]: the durable, fenced [`CursorStore`].

use async_trait::async_trait;
use deadpool_postgres::Pool;
use spot_coordinator::FencingToken;
use spot_core::SiteId;
use spot_mail::MailboxCursor;
use spot_mail_intake::{CursorStore, StoreError};

use crate::{conn, schema, sql, unavailable};

/// The UID cursor of each account of one site, in PostgreSQL.
///
/// Call [`PgCursorStore::prepare`] once at start-up. Clones share the pool.
#[derive(Clone)]
pub struct PgCursorStore {
    pool: Pool,
    site: SiteId,
}

impl std::fmt::Debug for PgCursorStore {
    /// Shows no connection detail: the pool's own `Debug` names the host and the database.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCursorStore").finish_non_exhaustive()
    }
}

impl PgCursorStore {
    /// A store for `site` on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool, site: SiteId) -> Self {
        Self { pool, site }
    }

    /// Creates the tables if they are missing; idempotent and safe from several nodes.
    ///
    /// # Errors
    /// `Unavailable` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        schema::prepare(&self.pool).await
    }
}

#[async_trait]
impl CursorStore for PgCursorStore {
    async fn load(&self, account: &str) -> Result<Option<MailboxCursor>, StoreError> {
        let connection = conn(&self.pool).await?;
        let row = connection
            .query_opt(sql::LOAD_CURSOR, &[&self.site.as_str(), &account])
            .await
            .map_err(|e| unavailable(&e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let uid_validity: i64 = row.get(0);
        let last_uid: i64 = row.get(1);
        // A value outside u32 is a row this version cannot read: unreadable, not guessed at.
        let uid_validity = u32::try_from(uid_validity).map_err(|_| StoreError::Unavailable)?;
        let last_uid = u32::try_from(last_uid).map_err(|_| StoreError::Unavailable)?;
        Ok(Some(MailboxCursor {
            uid_validity,
            last_uid,
        }))
    }

    async fn store(
        &self,
        account: &str,
        cursor: MailboxCursor,
        fence: FencingToken,
    ) -> Result<(), StoreError> {
        let fence = i64::try_from(fence.0).map_err(|_| StoreError::Unavailable)?;
        let uid_validity = i64::from(cursor.uid_validity);
        let last_uid = i64::from(cursor.last_uid);
        let connection = conn(&self.pool).await?;
        let changed = connection
            .execute(
                sql::STORE_CURSOR,
                &[
                    &self.site.as_str(),
                    &account,
                    &uid_validity,
                    &last_uid,
                    &fence,
                ],
            )
            .await
            .map_err(|e| unavailable(&e))?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::StaleFence)
        }
    }
}
