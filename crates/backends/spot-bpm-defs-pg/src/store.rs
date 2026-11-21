//! [`PgDefinitionStore`]: the PostgreSQL [`DefinitionStore`].

use deadpool_postgres::Pool;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_definitions::{
    DefFuture, DefinitionError, DefinitionStore, MAX_LABEL_BYTES, MAX_PAGE, StoredVersion,
    VersionState, check_name, check_publish,
};
use tokio_postgres::Row;

use crate::{error, schema, sql};

/// The tenant a store serves unless [`PgDefinitionStore::with_tenant`] says otherwise.
pub const DEFAULT_TENANT: &str = "default";

/// A [`DefinitionStore`] over PostgreSQL, for one tenant.
#[derive(Clone)]
pub struct PgDefinitionStore {
    pool: Pool,
    tenant: String,
}

impl PgDefinitionStore {
    /// A store on `pool` for the [`DEFAULT_TENANT`]. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            tenant: DEFAULT_TENANT.to_owned(),
        }
    }

    /// The same store for another tenant. The tenant is fixed here and is in every statement.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// The tenant every row is stamped with and every statement filters by.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables, the guard function and the trigger if missing.
    ///
    /// # Errors
    /// [`DefinitionError::StoreFailed`] when the database cannot apply the DDL.
    pub async fn prepare(&self) -> Result<(), DefinitionError> {
        schema::prepare(&self.pool).await
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, DefinitionError> {
        self.pool.get().await.map_err(|_| error::pool())
    }
}

/// Reads one stored version; a row that cannot be read back is an error, never skipped.
fn row(row: &Row, expected_name: &str) -> Result<StoredVersion, DefinitionError> {
    let name: String = row.try_get(0).map_err(|_| error::corrupt())?;
    let version: i32 = row.try_get(1).map_err(|_| error::corrupt())?;
    let state: String = row.try_get(4).map_err(|_| error::corrupt())?;
    let stored = StoredVersion {
        name,
        version: u32::try_from(version).map_err(|_| error::corrupt())?,
        text: row.try_get(2).map_err(|_| error::corrupt())?,
        content_hash: row.try_get(3).map_err(|_| error::corrupt())?,
        state: VersionState::parse(&state).ok_or_else(error::corrupt)?,
        published_by: row.try_get(5).map_err(|_| error::corrupt())?,
        published_at: row.try_get(6).map_err(|_| error::corrupt())?,
        note: row.try_get(7).map_err(|_| error::corrupt())?,
        retired_at: row.try_get(8).map_err(|_| error::corrupt())?,
        retired_by: row.try_get(9).map_err(|_| error::corrupt())?,
    };
    let retired = stored.state == VersionState::Retired;
    if stored.name != expected_name
        || stored.version == 0
        || retired != stored.retired_at.is_some()
        || retired != stored.retired_by.is_some()
    {
        return Err(error::corrupt());
    }
    Ok(stored)
}

fn check_actor(by: &str) -> Result<(), DefinitionError> {
    if by.is_empty() || by.contains('\0') {
        return Err(DefinitionError::InputInvalid { field: "publisher" });
    }
    if by.len() > MAX_LABEL_BYTES {
        return Err(DefinitionError::TooLarge {
            field: "publisher",
            len: by.len(),
            max: MAX_LABEL_BYTES,
        });
    }
    Ok(())
}

fn number(latest: i32) -> Option<u32> {
    u32::try_from(latest).ok().filter(|n| *n > 0)
}

impl DefinitionStore for PgDefinitionStore {
    fn publish<'a>(
        &'a self,
        name: &'a str,
        text: &'a str,
        expected_previous: Option<u32>,
        publisher: &'a str,
        note: Option<&'a str>,
        now: OffsetDateTime,
    ) -> DefFuture<'a, StoredVersion> {
        Box::pin(async move {
            let content_hash = check_publish(name, text, publisher, note)?;
            let mut client = self.client().await?;
            let tx = client.transaction().await.map_err(|e| error::pg(&e))?;
            tx.execute(sql::ENSURE_HEAD, &[&self.tenant, &name])
                .await
                .map_err(|e| error::pg(&e))?;
            // `Some(0)` is never a latest version: it can only conflict. A number beyond i32
            // cannot be the head either.
            let expected = match expected_previous {
                None => Some(0_i32),
                Some(n) if n > 0 => i32::try_from(n).ok(),
                Some(_) => None,
            };
            let won = match expected {
                Some(expected) => tx
                    .query_opt(sql::BUMP_HEAD, &[&self.tenant, &name, &expected])
                    .await
                    .map_err(|e| error::pg(&e))?,
                None => None,
            };
            let Some(won) = won else {
                let found = tx
                    .query_opt(sql::SELECT_HEAD, &[&self.tenant, &name])
                    .await
                    .map_err(|e| error::pg(&e))?
                    .map(|r| r.try_get::<_, i32>(0))
                    .transpose()
                    .map_err(|_| error::corrupt())?
                    .and_then(number);
                // Dropping the transaction rolls back, taking the zero head row with it.
                drop(tx);
                return Err(DefinitionError::VersionConflict {
                    name: name.to_owned(),
                    expected: expected_previous,
                    found,
                });
            };
            let version: i32 = won.try_get(0).map_err(|_| error::corrupt())?;
            tx.execute(
                sql::INSERT_VERSION,
                &[
                    &self.tenant,
                    &name,
                    &version,
                    &text,
                    &content_hash,
                    &publisher,
                    &now,
                    &note,
                ],
            )
            .await
            .map_err(|e| error::pg(&e))?;
            tx.commit().await.map_err(|e| error::pg(&e))?;
            Ok(StoredVersion {
                name: name.to_owned(),
                version: u32::try_from(version).map_err(|_| error::corrupt())?,
                text: text.to_owned(),
                content_hash,
                state: VersionState::Published,
                published_by: publisher.to_owned(),
                published_at: now,
                note: note.map(str::to_owned),
                retired_at: None,
                retired_by: None,
            })
        })
    }

    fn get<'a>(&'a self, name: &'a str, version: u32) -> DefFuture<'a, Option<StoredVersion>> {
        Box::pin(async move {
            check_name(name)?;
            let Ok(version) = i32::try_from(version) else {
                return Ok(None);
            };
            let client = self.client().await?;
            let found = client
                .query_opt(sql::SELECT_VERSION, &[&self.tenant, &name, &version])
                .await
                .map_err(|e| error::pg(&e))?;
            found.map(|r| row(&r, name)).transpose()
        })
    }

    fn latest_published<'a>(&'a self, name: &'a str) -> DefFuture<'a, Option<StoredVersion>> {
        Box::pin(async move {
            check_name(name)?;
            let client = self.client().await?;
            let found = client
                .query_opt(sql::SELECT_LATEST_PUBLISHED, &[&self.tenant, &name])
                .await
                .map_err(|e| error::pg(&e))?;
            found.map(|r| row(&r, name)).transpose()
        })
    }

    fn list<'a>(
        &'a self,
        name: &'a str,
        after: Option<u32>,
        limit: usize,
    ) -> DefFuture<'a, Vec<StoredVersion>> {
        Box::pin(async move {
            check_name(name)?;
            let after = i32::try_from(after.unwrap_or(0)).unwrap_or(i32::MAX);
            let limit = i64::try_from(limit.min(MAX_PAGE)).unwrap_or(0);
            let client = self.client().await?;
            let rows = client
                .query(sql::SELECT_PAGE, &[&self.tenant, &name, &after, &limit])
                .await
                .map_err(|e| error::pg(&e))?;
            rows.iter().map(|r| row(r, name)).collect()
        })
    }

    fn names<'a>(&'a self) -> DefFuture<'a, Vec<String>> {
        Box::pin(async move {
            let client = self.client().await?;
            let rows = client
                .query(sql::SELECT_NAMES, &[&self.tenant])
                .await
                .map_err(|e| error::pg(&e))?;
            rows.iter()
                .map(|r| {
                    let name: String = r.try_get(0).map_err(|_| error::corrupt())?;
                    check_name(&name).map_err(|_| error::corrupt())?;
                    Ok(name)
                })
                .collect()
        })
    }

    fn retire<'a>(
        &'a self,
        name: &'a str,
        version: u32,
        by: &'a str,
        now: OffsetDateTime,
    ) -> DefFuture<'a, StoredVersion> {
        Box::pin(async move {
            check_name(name)?;
            check_actor(by)?;
            let not_found = || DefinitionError::NotFound {
                name: name.to_owned(),
                version,
            };
            let Ok(number) = i32::try_from(version) else {
                return Err(not_found());
            };
            let client = self.client().await?;
            let changed = client
                .query_opt(sql::RETIRE, &[&self.tenant, &name, &number, &now, &by])
                .await
                .map_err(|e| error::pg(&e))?;
            if let Some(changed) = changed {
                return row(&changed, name);
            }
            // Not changed: already retired (return it as it is) or absent.
            let existing = client
                .query_opt(sql::SELECT_VERSION, &[&self.tenant, &name, &number])
                .await
                .map_err(|e| error::pg(&e))?;
            match existing {
                Some(existing) => row(&existing, name),
                None => Err(not_found()),
            }
        })
    }

    fn running_pins<'a>(&'a self, name: &'a str) -> DefFuture<'a, Vec<u32>> {
        Box::pin(async move {
            check_name(name)?;
            let client = self.client().await?;
            let exists: bool = client
                .query_one(sql::CASE_TABLE_EXISTS, &[])
                .await
                .map_err(|e| error::pg(&e))?
                .try_get(0)
                .map_err(|_| error::corrupt())?;
            if !exists {
                return Ok(Vec::new());
            }
            let rows = client
                .query(sql::SELECT_PINS, &[&self.tenant, &name])
                .await
                .map_err(|e| error::pg(&e))?;
            rows.iter()
                .map(|r| {
                    let v: i32 = r.try_get(0).map_err(|_| error::corrupt())?;
                    u32::try_from(v).map_err(|_| error::corrupt())
                })
                .collect()
        })
    }
}
