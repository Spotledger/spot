//! [`PgEnvironmentStore`]: the `EnvironmentStore` over the table of [`crate::schema`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use serde_json::Value;
use spot_environments::{
    EnvironmentError, EnvironmentSet, EnvironmentStore, Result, StoredSet, codes,
};
use spot_pg_bridge::Bridge;
use tokio_postgres::IsolationLevel;

use crate::codec::{decode, encode};
use crate::error::{limit, setup, store_failed};
use crate::schema;

/// The deadline of `get`: one indexed read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// The deadline of `create` and `replace`: one or two statements in a transaction.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// The deadline of `list`, which reads up to [`LIST_CEILING`] rows in pages.
const LIST_DEADLINE: Duration = Duration::from_secs(20);
/// The deadline of the schema preparation.
const SCHEMA_DEADLINE: Duration = Duration::from_secs(30);

/// Rows read per statement by `list`.
pub const LIST_PAGE: usize = 100;
/// The most sets `list` returns. More than this fails with `environments.store_failed` rather
/// than returning a truncated answer; the trait returns every set, so a product with this many
/// sets needs a paged trait method first (an open question on the task).
pub const LIST_CEILING: usize = 5_000;

/// A stored row as read: name, version and document.
type Row = (String, i64, Value);

/// The environment store over PostgreSQL. Use it from a control-plane process, a CLI command or
/// a job worker; never from a request handler and never inside a transaction an async store
/// holds (see the crate docs).
#[derive(Clone)]
pub struct PgEnvironmentStore {
    bridge: Arc<Bridge>,
}

impl PgEnvironmentStore {
    /// A store that reaches the database through `bridge`, the process's shared bridge, built
    /// on `pool`.
    ///
    /// # Errors
    /// `environments_pg.setup_failed` when the bridge configuration does not hold against the
    /// pool's size, or when called on a current-thread tokio runtime (the bridge's blocking
    /// calls could never run there).
    pub fn new(bridge: Arc<Bridge>, pool: &Pool) -> std::result::Result<Self, EnvironmentError> {
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                setup("the bridge configuration does not fit the pool size; see the README")
            })?;
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        {
            return Err(setup(
                "the store cannot be built on a current-thread runtime; build it on a plain thread",
            ));
        }
        Ok(Self { bridge })
    }

    /// Creates the schema and table if they are missing (idempotent, safe from several
    /// processes at once). A host calls it once at start-up.
    ///
    /// # Errors
    /// `environments_pg.setup_failed`.
    pub fn ensure_schema(&self) -> std::result::Result<(), EnvironmentError> {
        schema::ensure(&self.bridge, SCHEMA_DEADLINE)
    }
}

impl fmt::Debug for PgEnvironmentStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgEnvironmentStore").finish_non_exhaustive()
    }
}

fn not_found(name: &str) -> EnvironmentError {
    EnvironmentError::about(
        codes::STORE_NOT_FOUND,
        name,
        "no environment set with that name exists",
    )
}

fn version_of(raw: i64) -> Result<u64> {
    u64::try_from(raw)
        .ok()
        .filter(|v| *v >= 1)
        .ok_or_else(crate::error::row_invalid)
}

fn stored(row: Row) -> Result<StoredSet> {
    let (name, version, doc) = row;
    Ok(StoredSet {
        set: decode(&name, doc)?,
        version: version_of(version)?,
    })
}

impl EnvironmentStore for PgEnvironmentStore {
    fn create(&self, set: EnvironmentSet) -> Result<StoredSet> {
        set.validate()?;
        let doc = encode(&set)?;
        let name = set.name().to_owned();
        let inserted = self
            .bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move { conn.execute(INSERT, &[&name, &doc]).await })
            })
            .map_err(|e| store_failed(&e))?;
        if inserted == 1 {
            Ok(StoredSet { set, version: 1 })
        } else {
            Err(EnvironmentError::about(
                codes::STORE_NAME_TAKEN,
                set.name(),
                "an environment set with that name already exists",
            ))
        }
    }

    fn get(&self, name: &str) -> Result<StoredSet> {
        let key = name.to_owned();
        let row = self
            .bridge
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn
                        .query_opt(
                            "SELECT name, version, doc FROM spot_control.spot_environment_set \
                             WHERE name = $1",
                            &[&key],
                        )
                        .await?;
                    row.map(|r| {
                        Ok::<Row, tokio_postgres::Error>((
                            r.try_get(0)?,
                            r.try_get(1)?,
                            r.try_get(2)?,
                        ))
                    })
                    .transpose()
                })
            })
            .map_err(|e| store_failed(&e))?;
        row.map_or_else(|| Err(not_found(name)), stored)
    }

    fn replace(&self, set: EnvironmentSet, expected_version: u64) -> Result<StoredSet> {
        set.validate()?;
        let doc = encode(&set)?;
        let name = set.name().to_owned();
        let Ok(expected) = i64::try_from(expected_version) else {
            // No stored version can be this large, so it can only be missing or a conflict.
            return self.get(set.name()).and_then(|_| {
                Err(EnvironmentError::about(
                    codes::STORE_VERSION_CONFLICT,
                    set.name(),
                    "the set changed since it was read",
                ))
            });
        };
        let outcome = self
            .bridge
            .run(WRITE_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let updated = tx
                        .execute(
                            "UPDATE spot_control.spot_environment_set \
                             SET doc = $2, version = version + 1 \
                             WHERE name = $1 AND version = $3",
                            &[&name, &doc, &expected],
                        )
                        .await?;
                    let outcome = if updated == 1 {
                        Outcome::Replaced
                    } else if tx
                        .query_opt(
                            "SELECT 1 FROM spot_control.spot_environment_set WHERE name = $1",
                            &[&name],
                        )
                        .await?
                        .is_some()
                    {
                        Outcome::Conflict
                    } else {
                        Outcome::Missing
                    };
                    tx.commit().await?;
                    Ok::<Outcome, tokio_postgres::Error>(outcome)
                })
            })
            .map_err(|e| store_failed(&e))?;
        match outcome {
            Outcome::Replaced => Ok(StoredSet {
                set,
                version: expected_version + 1,
            }),
            Outcome::Conflict => Err(EnvironmentError::about(
                codes::STORE_VERSION_CONFLICT,
                set.name(),
                "the set changed since it was read",
            )),
            Outcome::Missing => Err(not_found(set.name())),
        }
    }

    fn list(&self) -> Result<Vec<StoredSet>> {
        let rows = self
            .bridge
            .run(LIST_DEADLINE, |mut conn| {
                Box::pin(async move {
                    // One snapshot for every page, so a concurrent write cannot tear the list.
                    let tx = conn
                        .build_transaction()
                        .isolation_level(IsolationLevel::RepeatableRead)
                        .read_only(true)
                        .start()
                        .await?;
                    let page = i64::try_from(LIST_PAGE).unwrap_or(100);
                    let mut rows: Vec<Row> = Vec::new();
                    let mut after = String::new();
                    loop {
                        let batch = tx
                            .query(
                                "SELECT name, version, doc FROM spot_control.spot_environment_set \
                                 WHERE name > $1 ORDER BY name LIMIT $2",
                                &[&after, &page],
                            )
                            .await?;
                        let full = batch.len() == LIST_PAGE;
                        for r in &batch {
                            rows.push((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?));
                        }
                        if rows.len() > LIST_CEILING {
                            break;
                        }
                        match rows.last() {
                            Some(last) if full => after.clone_from(&last.0),
                            _ => break,
                        }
                    }
                    tx.commit().await?;
                    Ok::<Vec<Row>, tokio_postgres::Error>(rows)
                })
            })
            .map_err(|e| store_failed(&e))?;
        if rows.len() > LIST_CEILING {
            return Err(limit(
                "there are more environment sets than one list returns; nothing was truncated",
            ));
        }
        rows.into_iter().map(stored).collect()
    }
}

/// What a replace statement found.
enum Outcome {
    Replaced,
    Conflict,
    Missing,
}

const INSERT: &str = "INSERT INTO spot_control.spot_environment_set (name, version, doc) \
                      VALUES ($1, 1, $2) ON CONFLICT (name) DO NOTHING";
