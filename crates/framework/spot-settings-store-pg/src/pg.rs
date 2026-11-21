//! The PostgreSQL [`SettingsBackend`].
//!
//! Two tables, both keyed by `(scope, key)`: `spot_settings_value` holds the current value and
//! its version, `spot_settings_history` the append-only history. A write is one transaction on one
//! connection: the value row, then the history row. Every statement is parameterised. A removal
//! keeps the version counter in the history, so a stale writer never matches a recreated row.

use deadpool_postgres::Pool;
use spot_settings::ScopeRef;

use crate::digest;
use crate::{
    BoxFuture, CORRUPT_ROW, HistoryRow, STORAGE_FAILED, SetRequest, SettingsBackend, StoreError,
    Stored, VERSION_CONFLICT,
};

/// The idempotent DDL.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_settings_value (
    scope   text NOT NULL,
    key     text NOT NULL,
    kind    text NOT NULL CHECK (kind IN ('value', 'secret_ref')),
    body    text NOT NULL,
    version bigint NOT NULL CHECK (version >= 1),
    PRIMARY KEY (scope, key)
);

CREATE TABLE IF NOT EXISTS spot_settings_history (
    scope      text NOT NULL,
    key        text NOT NULL,
    version    bigint NOT NULL,
    actor      text NOT NULL,
    channel    text NOT NULL,
    at         bigint NOT NULL,
    old_digest text,
    new_digest text,
    PRIMARY KEY (scope, key, version)
);
"#;

fn db(operation: &str, error: &tokio_postgres::Error) -> StoreError {
    let state = error
        .code()
        .map_or_else(|| "no sqlstate".to_owned(), |c| c.code().to_owned());
    StoreError::new(
        STORAGE_FAILED,
        format!("settings store: {operation} failed (sqlstate {state})"),
    )
}

fn pool_error(operation: &str) -> StoreError {
    StoreError::new(
        STORAGE_FAILED,
        format!("settings store: {operation} found no database connection"),
    )
}

fn corrupt(what: &str) -> StoreError {
    StoreError::new(CORRUPT_ROW, format!("settings store: {what}"))
}

fn to_u64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| corrupt("a negative version"))
}

/// The PostgreSQL store over a pool the caller builds.
#[derive(Clone)]
pub struct PgSettingsStore {
    pool: Pool,
}

impl std::fmt::Debug for PgSettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSettingsStore").finish_non_exhaustive()
    }
}

impl PgSettingsStore {
    /// A store over `pool`; call [`prepare`](Self::prepare) once before use.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the tables when missing (idempotent).
    ///
    /// # Errors
    /// `settings_store.storage_failed` when the DDL cannot be applied.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(|_| pool_error("prepare"))?;
        client
            .batch_execute(DDL)
            .await
            .map_err(|e| db("create the tables", &e))
    }
}

fn decode(kind: &str, body: &str, version: i64) -> Result<Stored, StoreError> {
    let value =
        digest::restore(kind, body).ok_or_else(|| corrupt("a value cannot be read back"))?;
    Ok(Stored {
        value,
        version: to_u64(version)?,
    })
}

impl SettingsBackend for PgSettingsStore {
    fn get<'a>(
        &'a self,
        scope: &'a ScopeRef,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Stored>, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| pool_error("get"))?;
            let row = client
                .query_opt(
                    "SELECT kind, body, version FROM spot_settings_value WHERE scope = $1 AND key = $2",
                    &[&scope.canonical(), &key],
                )
                .await
                .map_err(|e| db("read a value", &e))?;
            row.map(|r| {
                decode(
                    &r.get::<_, String>(0),
                    &r.get::<_, String>(1),
                    r.get::<_, i64>(2),
                )
            })
            .transpose()
        })
    }

    fn list<'a>(
        &'a self,
        scope: &'a ScopeRef,
    ) -> BoxFuture<'a, Result<Vec<(String, Stored)>, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| pool_error("list"))?;
            let rows = client
                .query(
                    "SELECT key, kind, body, version FROM spot_settings_value WHERE scope = $1 ORDER BY key",
                    &[&scope.canonical()],
                )
                .await
                .map_err(|e| db("list values", &e))?;
            rows.iter()
                .map(|r| {
                    decode(
                        &r.get::<_, String>(1),
                        &r.get::<_, String>(2),
                        r.get::<_, i64>(3),
                    )
                    .map(|s| (r.get::<_, String>(0), s))
                })
                .collect()
        })
    }

    fn set<'a>(&'a self, request: &'a SetRequest) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut client = self.pool.get().await.map_err(|_| pool_error("set"))?;
            let tx = client.transaction().await.map_err(|e| db("begin", &e))?;
            let scope = request.scope.canonical();
            let current = tx
                .query_opt(
                    "SELECT kind, body, version FROM spot_settings_value WHERE scope = $1 AND key = $2 FOR UPDATE",
                    &[&scope, &request.key],
                )
                .await
                .map_err(|e| db("lock the value", &e))?;
            let (old_digest, version) = match &current {
                Some(row) => (
                    Some(digest::digest_of(
                        &row.get::<_, String>(0),
                        &row.get::<_, String>(1),
                    )),
                    to_u64(row.get::<_, i64>(2))?,
                ),
                None => (None, 0),
            };
            if request.expected.is_some_and(|e| e != version) {
                return Err(StoreError::new(
                    VERSION_CONFLICT,
                    "the value changed since it was read",
                ));
            }
            // The counter survives a removal: the next version follows the history's highest.
            let highest: Option<i64> = tx
                .query_one(
                    "SELECT max(version) FROM spot_settings_history WHERE scope = $1 AND key = $2",
                    &[&scope, &request.key],
                )
                .await
                .map_err(|e| db("read the history head", &e))?
                .get(0);
            let next = to_u64(highest.unwrap_or(0))?.max(version) + 1;
            let next_db = i64::try_from(next).map_err(|_| corrupt("a version out of range"))?;
            let new_digest = request.value.as_ref().map(digest::of);
            match &request.value {
                Some(value) => {
                    let (kind, body) = digest::persisted(value);
                    if current.is_some() {
                        tx.execute(
                            "UPDATE spot_settings_value SET kind = $3, body = $4, version = $5 WHERE scope = $1 AND key = $2",
                            &[&scope, &request.key, &kind, &body, &next_db],
                        )
                        .await
                        .map_err(|e| db("update the value", &e))?;
                    } else {
                        let inserted = tx
                            .execute(
                                "INSERT INTO spot_settings_value (scope, key, kind, body, version) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                                &[&scope, &request.key, &kind, &body, &next_db],
                            )
                            .await
                            .map_err(|e| db("insert the value", &e))?;
                        if inserted == 0 {
                            return Err(StoreError::new(
                                VERSION_CONFLICT,
                                "the value changed since it was read",
                            ));
                        }
                    }
                }
                None => {
                    tx.execute(
                        "DELETE FROM spot_settings_value WHERE scope = $1 AND key = $2",
                        &[&scope, &request.key],
                    )
                    .await
                    .map_err(|e| db("remove the value", &e))?;
                }
            }
            tx.execute(
                "INSERT INTO spot_settings_history (scope, key, version, actor, channel, at, old_digest, new_digest) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &scope,
                    &request.key,
                    &next_db,
                    &request.actor,
                    &request.channel,
                    &request.at,
                    &old_digest,
                    &new_digest,
                ],
            )
            .await
            .map_err(|e| db("write the history", &e))?;
            tx.commit().await.map_err(|e| db("commit", &e))?;
            Ok(next)
        })
    }

    fn history<'a>(
        &'a self,
        scope: &'a ScopeRef,
        key: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<HistoryRow>, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| pool_error("history"))?;
            let limit = i64::try_from(limit.min(1000)).unwrap_or(1000);
            let rows = client
                .query(
                    "SELECT version, actor, channel, at, old_digest, new_digest FROM spot_settings_history WHERE scope = $1 AND key = $2 ORDER BY version DESC LIMIT $3",
                    &[&scope.canonical(), &key, &limit],
                )
                .await
                .map_err(|e| db("read the history", &e))?;
            rows.iter()
                .map(|r| {
                    Ok(HistoryRow {
                        version: to_u64(r.get::<_, i64>(0))?,
                        actor: r.get(1),
                        channel: r.get(2),
                        at: r.get(3),
                        old_digest: r.get(4),
                        new_digest: r.get(5),
                    })
                })
                .collect()
        })
    }
}
