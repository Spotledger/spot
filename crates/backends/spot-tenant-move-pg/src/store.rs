//! [`PgMoveStore`]: the `MoveStore` over PostgreSQL through the shared bridge.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_pg_bridge::{Bridge, PooledConn};
use spot_tenancy::TenantId;
use spot_tenant_move::{Error, MoveRecord, MoveStore, Result};

use crate::error::{SetupError, from_bridge, is_unique_violation, query_failed, store_failed};
use crate::schema;

/// Deadline of a read (`get`).
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a write (`create`, `save`), one transaction.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

const INSERT_MOVE: &str = "INSERT INTO spot_control.spot_tenant_move \
    (tenant, revision, record, state, source_cluster, target_cluster, updated_at) \
    VALUES ($1, 0, $2::text::jsonb, $3, $4, $5, $6)";

const UPDATE_MOVE: &str = "UPDATE spot_control.spot_tenant_move \
    SET revision = revision + 1, record = $3::text::jsonb, state = $4, \
        source_cluster = $5, target_cluster = $6, updated_at = $7 \
    WHERE tenant = $1 AND revision = $2";

const SELECT_MOVE: &str =
    "SELECT record::text, revision FROM spot_control.spot_tenant_move WHERE tenant = $1";

const EXISTS_MOVE: &str = "SELECT 1 FROM spot_control.spot_tenant_move WHERE tenant = $1";

const COUNT_EVENTS: &str =
    "SELECT count(*) FROM spot_control.spot_tenant_move_event WHERE tenant = $1";

const INSERT_EVENT: &str = "INSERT INTO spot_control.spot_tenant_move_event \
    (tenant, seq, state, at) VALUES ($1, $2, $3, $4)";

/// The PostgreSQL move store. Build it with [`PgMoveStore::new`], create the tables once with
/// [`PgMoveStore::prepare`], then hand it to the engine as a `&dyn MoveStore`.
///
/// **Use it only from a control-plane process, a CLI command or a job worker, never from a
/// request handler**, and never inside a transaction an async store holds (ADR-082 section 2).
pub struct PgMoveStore {
    bridge: Arc<Bridge>,
}

impl std::fmt::Debug for PgMoveStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMoveStore").finish_non_exhaustive()
    }
}

/// One validated, ready-to-bind view of a record.
struct Row {
    tenant: String,
    document: String,
    state: &'static str,
    source: String,
    target: String,
    updated_at: i64,
}

impl Row {
    fn of(record: &MoveRecord) -> Result<Self> {
        let tenant = record.plan.tenant.to_string();
        let document = serde_json::to_string(record)
            .map_err(|_| store_failed(&tenant, "the move record could not be encoded"))?;
        let updated_at = i64::try_from(record.updated_at)
            .map_err(|_| store_failed(&tenant, "the move timestamp is out of range"))?;
        Ok(Self {
            document,
            state: record.state.name(),
            source: record.plan.from.cluster.as_str().to_owned(),
            target: record.plan.to.cluster.as_str().to_owned(),
            updated_at,
            tenant,
        })
    }
}

impl PgMoveStore {
    /// Builds the store on the process's shared `bridge` over `pool`.
    ///
    /// `pool` is the pool the bridge was built on; the bridge configuration is validated against
    /// its size again here, so a pool swapped for a smaller one is refused. The sizing (see the
    /// README): a move store makes one call at a time per operator, so the bridge's
    /// `max_in_flight` only has to cover the operators and job workers sharing this process.
    ///
    /// # Errors
    /// `tenant_move_pg.bad_setup` when the bridge configuration cannot hold against the pool, or
    /// when the caller is inside a current-thread tokio runtime (every call would be refused
    /// with `pg_bridge.wrong_thread`, and a current-thread runtime cannot be left safely).
    pub fn new(bridge: Arc<Bridge>, pool: &Pool) -> std::result::Result<Self, SetupError> {
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        {
            return Err(SetupError::bad_setup(
                "the move store cannot be built inside a current-thread runtime",
            ));
        }
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                SetupError::bad_setup("the bridge configuration does not hold against the pool")
            })?;
        Ok(Self { bridge })
    }

    /// Creates the control tables if they are missing, in one transaction under an advisory lock.
    ///
    /// # Errors
    /// `tenant_move_pg.migration_failed` with a fixed sentence.
    pub fn prepare(&self) -> std::result::Result<(), SetupError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move { schema::prepare(&mut conn).await.map_err(|_| ()) })
            })
            .map_err(|_| SetupError::migration("the move tables could not be created"))
    }
}

impl MoveStore for PgMoveStore {
    fn create(&self, mut record: MoveRecord) -> Result<()> {
        record.revision = 0;
        let row = Row::of(&record)?;
        let tenant = row.tenant.clone();
        let events = events_of(&record, &tenant)?;
        from_bridge(
            &tenant,
            self.bridge
                .run(WRITE_DEADLINE, move |mut conn: PooledConn| {
                    Box::pin(async move {
                        let tx = conn
                            .transaction()
                            .await
                            .map_err(|_| query_failed(&row.tenant))?;
                        let inserted = tx
                            .execute(
                                INSERT_MOVE,
                                &[
                                    &row.tenant,
                                    &row.document,
                                    &row.state,
                                    &row.source,
                                    &row.target,
                                    &row.updated_at,
                                ],
                            )
                            .await;
                        if let Err(error) = inserted {
                            return Err(if is_unique_violation(&error) {
                                Error::AlreadyExists { tenant: row.tenant }
                            } else {
                                query_failed(&row.tenant)
                            });
                        }
                        write_events(&tx, &row.tenant, 0, &events).await?;
                        tx.commit().await.map_err(|_| query_failed(&row.tenant))
                    })
                }),
        )
    }

    fn get(&self, tenant: &TenantId) -> Result<Option<MoveRecord>> {
        let key = tenant.to_string();
        let wanted = key.clone();
        from_bridge(
            &key,
            self.bridge.run(READ_DEADLINE, move |conn: PooledConn| {
                Box::pin(async move {
                    let row = conn
                        .query_opt(SELECT_MOVE, &[&wanted])
                        .await
                        .map_err(|_| query_failed(&wanted))?;
                    let Some(row) = row else { return Ok(None) };
                    let document: String = row.try_get(0).map_err(|_| query_failed(&wanted))?;
                    let revision: i64 = row.try_get(1).map_err(|_| query_failed(&wanted))?;
                    let mut record: MoveRecord = serde_json::from_str(&document).map_err(|_| {
                        store_failed(&wanted, "the stored move record cannot be read")
                    })?;
                    if record.plan.tenant.to_string() != wanted {
                        return Err(store_failed(
                            &wanted,
                            "the stored move record belongs to another tenant",
                        ));
                    }
                    record.revision = u64::try_from(revision)
                        .map_err(|_| store_failed(&wanted, "the stored revision is invalid"))?;
                    Ok(Some(record))
                })
            }),
        )
    }

    fn save(&self, mut record: MoveRecord, expected_revision: u64) -> Result<MoveRecord> {
        let tenant = record.plan.tenant.to_string();
        let expected = i64::try_from(expected_revision)
            .ok()
            .and_then(|r| r.checked_add(1).map(|_| r))
            .ok_or_else(|| store_failed(&tenant, "the expected revision is out of range"))?;
        record.revision = expected_revision.saturating_add(1);
        let row = Row::of(&record)?;
        let events = events_of(&record, &tenant)?;
        let saved = record.clone();
        from_bridge(
            &tenant,
            self.bridge
                .run(WRITE_DEADLINE, move |mut conn: PooledConn| {
                    Box::pin(async move {
                        let tx = conn
                            .transaction()
                            .await
                            .map_err(|_| query_failed(&row.tenant))?;
                        let changed = tx
                            .execute(
                                UPDATE_MOVE,
                                &[
                                    &row.tenant,
                                    &expected,
                                    &row.document,
                                    &row.state,
                                    &row.source,
                                    &row.target,
                                    &row.updated_at,
                                ],
                            )
                            .await
                            .map_err(|_| query_failed(&row.tenant))?;
                        if changed == 0 {
                            let exists = tx
                                .query_opt(EXISTS_MOVE, &[&row.tenant])
                                .await
                                .map_err(|_| query_failed(&row.tenant))?;
                            return Err(if exists.is_some() {
                                store_failed(&row.tenant, "the move changed since it was read")
                            } else {
                                store_failed(&row.tenant, "the move disappeared from the store")
                            });
                        }
                        let stored: i64 = tx
                            .query_one(COUNT_EVENTS, &[&row.tenant])
                            .await
                            .and_then(|r| r.try_get(0))
                            .map_err(|_| query_failed(&row.tenant))?;
                        let stored = usize::try_from(stored).map_err(|_| {
                            store_failed(&row.tenant, "the stored history is invalid")
                        })?;
                        if stored > events.len() {
                            return Err(store_failed(
                                &row.tenant,
                                "the move history is shorter than the stored one",
                            ));
                        }
                        write_events(&tx, &row.tenant, stored, &events).await?;
                        tx.commit().await.map_err(|_| query_failed(&row.tenant))?;
                        Ok(saved)
                    })
                }),
        )
    }
}

/// The history as `(state name, at)` pairs, with `at` range-checked.
fn events_of(record: &MoveRecord, tenant: &str) -> Result<Vec<(&'static str, i64)>> {
    record
        .history
        .iter()
        .map(|event| {
            i64::try_from(event.at)
                .map(|at| (event.state.name(), at))
                .map_err(|_| store_failed(tenant, "the move timestamp is out of range"))
        })
        .collect()
}

/// Appends the history rows from index `from` on, inside the caller's transaction.
async fn write_events(
    tx: &tokio_postgres::Transaction<'_>,
    tenant: &str,
    from: usize,
    events: &[(&'static str, i64)],
) -> Result<()> {
    for (index, (state, at)) in events.iter().enumerate().skip(from) {
        let seq =
            i64::try_from(index).map_err(|_| store_failed(tenant, "the history is too long"))?;
        tx.execute(INSERT_EVENT, &[&tenant, &seq, state, at])
            .await
            .map_err(|_| query_failed(tenant))?;
    }
    Ok(())
}
