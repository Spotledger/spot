//! [`PgExecStore`]: the PostgreSQL [`ExecStore`], over the `spot_dag_run` and `spot_dag_node_state` tables.

use std::collections::BTreeMap;
use std::sync::Arc;

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_dag::NodeId;
use spot_dag_exec::rules::{already_applied, apply_step};
use spot_dag_exec::unit::WorkFuture;
use spot_dag_exec::{
    ExecError, ExecFuture, ExecResult, ExecStore, ExecStoreFactory, NodeState, ReadyRun, Run,
    RunId, RunPlan, Transition, Unit,
};
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{Catalog, Isolation, Name, StorageBackend, Transaction, TxOptions};
use tokio_postgres::{Row, Transaction as ConnTransaction};

use crate::convert;
use crate::error;
use crate::schema;
use crate::sql;
use crate::state;

/// The longest a caller-supplied business key may be (the T1 rule).
const MAX_KEY_LEN: usize = 256;

/// The DAG run state in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgExecStore::prepare`] once at
/// start-up; a store shares its pool with every clone.
#[derive(Clone)]
pub struct PgExecStore {
    pool: Pool,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl std::fmt::Debug for PgExecStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgExecStore").finish_non_exhaustive()
    }
}

impl PgExecStore {
    /// A store on `pool` whose transitions can also run on a storage transaction.
    ///
    /// `pool` serves the store's own reads and writes; `backend` (the storage backend for the
    /// same database) opens the [`PgTransaction`] that [`PgExecStore::apply`] and
    /// [`Unit::commit_with`] write on, and `catalog` is the tenant metadata those transactions
    /// declare. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool, backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            pool,
            backend,
            catalog,
        }
    }

    /// Creates the tables and indexes if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `dag_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> ExecResult<Object> {
        self.pool.get().await.map_err(error::pool)
    }

    /// The tenant this store is scoped to: the database name, because one tenant is one
    /// database (ADR-34). Every query of the store filters on it; it is never taken from the
    /// run id or from a transaction the caller passes in.
    #[must_use]
    pub fn tenant(&self) -> &str {
        self.backend.config().dbname.as_str()
    }

    /// The tenant, failing closed when the store has none.
    fn scope(&self) -> ExecResult<&str> {
        let tenant = self.tenant();
        if tenant.trim().is_empty() {
            return Err(error::corrupt("the store has no tenant scope"));
        }
        Ok(tenant)
    }

    /// Opens a read-write storage transaction for one transition.
    ///
    /// The transaction is the caller-visible unit of work: reads on it see the store's tables
    /// and the document engine's, and it commits or rolls back as a whole. It is one connection
    /// from the storage backend's pool, never a second checkout.
    async fn begin(&self, label: &str) -> ExecResult<PgTransaction> {
        let name = Name::new(label)?;
        let options = TxOptions::new(name, Isolation::ReadCommitted);
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(ExecError::StorageFailed)
    }

    /// Applies `transition` at `expected_version` on the caller's transaction, storing the run
    /// and its changed nodes but **not** committing: the caller commits or rolls back.
    ///
    /// This is the whole of the `apply` body on a transaction the caller owns. It takes the run
    /// row `FOR UPDATE` (so two applies of one run serialise), checks the
    /// `(run, node, attempt)` idempotency key first (a retried completion is a no-op), then the
    /// optimistic version (`dag_exec.version_conflict`), applies the transition and bumps the
    /// version. It opens no second connection, so a document written on the same transaction
    /// commits or rolls back with the run.
    ///
    /// # Errors
    /// `dag_exec.run_not_found`, `dag_exec.version_conflict`, `dag_exec.invalid_node`,
    /// `dag_exec.invalid_state`, and `dag_exec.storage_failed` when the database fails.
    pub async fn apply_in(
        &self,
        tx: &mut PgTransaction,
        transition: &Transition,
        expected_version: u64,
    ) -> ExecResult<Run> {
        // The tenant is the store's own scope. A transaction opened against another tenant's
        // database is refused before any statement runs on it.
        let tenant = self.scope()?.to_owned();
        if tx.tenant() != tenant {
            return Err(error::corrupt(
                "the transaction belongs to another tenant than the store",
            ));
        }
        // One lock per run serialises applies, so the version check cannot race.
        let row = tx
            .raw_query_opt(
                sql::SELECT_RUN_FOR_UPDATE,
                &[&transition.run.as_str(), &tenant],
                None,
            )
            .await?;
        let Some(row) = row else {
            return Err(ExecError::RunNotFound {
                id: transition.run.to_string(),
            });
        };
        let mut run = convert::run_from_row(&row)?;
        let nodes = tx
            .raw_query(
                sql::SELECT_NODES,
                &[&transition.run.as_str(), &tenant],
                None,
            )
            .await?;
        fill_nodes(&mut run, &nodes)?;

        // The idempotency key wins over the version check: a retried completion is a no-op.
        if already_applied(&run, &transition.step) {
            return Ok(run);
        }
        if run.version != expected_version {
            return Err(ExecError::VersionConflict {
                current: run.version,
                expected: expected_version,
            });
        }

        let mut updated = run;
        apply_step(&mut updated, &transition.step)?;
        updated.version += 1;
        update_run(tx, &tenant, &updated).await?;
        for node in state::touched(&transition.step) {
            if let Some(node_state) = updated.nodes.get(node) {
                update_node(tx, &tenant, &updated.id, node, node_state).await?;
            }
        }
        Ok(updated)
    }

    /// Loads a run and its nodes through `conn` or an open transaction.
    async fn load_run_with(
        conn: &tokio_postgres::Client,
        tenant: &str,
        id: &RunId,
    ) -> ExecResult<Option<Run>> {
        let row = conn
            .query_opt(sql::SELECT_RUN, &[&id.as_str(), &tenant])
            .await
            .map_err(error::pg)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut run = convert::run_from_row(&row)?;
        let nodes = conn
            .query(sql::SELECT_NODES, &[&id.as_str(), &tenant])
            .await
            .map_err(error::pg)?;
        fill_nodes(&mut run, &nodes)?;
        Ok(Some(run))
    }
}

fn fill_nodes(run: &mut Run, rows: &[Row]) -> ExecResult<()> {
    let mut nodes = BTreeMap::new();
    for row in rows {
        let (id, state) = convert::node_from_row(row)?;
        nodes.insert(id, state);
    }
    run.nodes = nodes;
    Ok(())
}

fn version_to_db(version: u64) -> ExecResult<i64> {
    i64::try_from(version).map_err(|_| error::corrupt("run version is out of range"))
}

fn attempts_to_db(attempts: u32) -> ExecResult<i32> {
    i32::try_from(attempts).map_err(|_| error::corrupt("node attempt count is out of range"))
}

fn validate_key(key: &str) -> ExecResult<()> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(ExecError::KeyInvalid);
    }
    Ok(())
}

/// Maps a failed `spot_dag_run` insert: a unique violation is the run id or the business key.
fn map_insert(error: tokio_postgres::Error, run: &Run) -> ExecError {
    if let Some(db) = error.as_db_error()
        && db.code().code() == "23505"
    {
        return match db.constraint() {
            Some("spot_dag_run_key_uniq") => ExecError::KeyConflict {
                key: run.key.clone(),
            },
            _ => ExecError::RunExists {
                id: run.id.to_string(),
            },
        };
    }
    error::pg(error)
}

async fn insert_run(tx: &ConnTransaction<'_>, tenant: &str, run: &Run) -> ExecResult<()> {
    let taken = convert::taken_to_json(&run.taken)?;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 11] = [
        &run.id.as_str(),
        &tenant,
        &run.key,
        &run.dag_key,
        &i32::try_from(run.dag_version).unwrap_or(i32::MAX),
        &run.content_hash,
        &convert::run_status_text(run.status),
        &convert::at(run.started_at)?,
        &run.ended_at.map(convert::at).transpose()?,
        &version_to_db(run.version)?,
        &taken,
    ];
    tx.execute(sql::INSERT_RUN, &params)
        .await
        .map_err(|e| map_insert(e, run))?;
    Ok(())
}

async fn insert_node(
    tx: &ConnTransaction<'_>,
    tenant: &str,
    run: &RunId,
    node: &NodeId,
    state: &NodeState,
) -> ExecResult<()> {
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 9] = [
        &run.as_str(),
        &tenant,
        &node.as_str(),
        &convert::node_status_text(state.status),
        &attempts_to_db(state.attempts)?,
        &state.output,
        &state.failure,
        &state.started_at.map(convert::at).transpose()?,
        &state.ended_at.map(convert::at).transpose()?,
    ];
    tx.execute(sql::INSERT_NODE, &params)
        .await
        .map_err(error::pg)?;
    Ok(())
}

async fn update_run(tx: &mut PgTransaction, tenant: &str, run: &Run) -> ExecResult<()> {
    let taken = convert::taken_to_json(&run.taken)?;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] = [
        &run.id.as_str(),
        &convert::run_status_text(run.status),
        &run.ended_at.map(convert::at).transpose()?,
        &version_to_db(run.version)?,
        &taken,
        &tenant,
    ];
    tx.raw_execute(sql::UPDATE_RUN, &params).await?;
    Ok(())
}

async fn update_node(
    tx: &mut PgTransaction,
    tenant: &str,
    run: &RunId,
    node: &NodeId,
    state: &NodeState,
) -> ExecResult<()> {
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 9] = [
        &run.as_str(),
        &node.as_str(),
        &convert::node_status_text(state.status),
        &attempts_to_db(state.attempts)?,
        &state.output,
        &state.failure,
        &state.started_at.map(convert::at).transpose()?,
        &state.ended_at.map(convert::at).transpose()?,
        &tenant,
    ];
    tx.raw_execute(sql::UPDATE_NODE, &params).await?;
    Ok(())
}

impl ExecStore for PgExecStore {
    fn create_run<'a>(&'a self, plan: RunPlan) -> ExecFuture<'a, Run> {
        Box::pin(async move {
            validate_key(&plan.run.key)?;
            let tenant = self.scope()?.to_owned();
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(error::pg)?;
            insert_run(&tx, &tenant, &plan.run).await?;
            for (node, state) in &plan.run.nodes {
                insert_node(&tx, &tenant, &plan.run.id, node, state).await?;
            }
            tx.commit().await.map_err(error::pg)?;
            Ok(plan.run)
        })
    }

    fn load_run<'a>(&'a self, id: &'a RunId) -> ExecFuture<'a, Option<Run>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            Self::load_run_with(&conn, self.scope()?, id).await
        })
    }

    fn apply<'a>(
        &'a self,
        transition: &'a Transition,
        expected_version: u64,
    ) -> ExecFuture<'a, Run> {
        Box::pin(async move {
            let mut tx = self.begin("dag_apply").await?;
            let run = self.apply_in(&mut tx, transition, expected_version).await?;
            tx.commit().await.map_err(ExecError::StorageFailed)?;
            Ok(run)
        })
    }

    fn list_ready<'a>(&'a self, limit: u32) -> ExecFuture<'a, Vec<ReadyRun>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(error::pg)?;
            let rows = tx
                .query(sql::LIST_READY_RUNS, &[&self.scope()?, &i64::from(limit)])
                .await
                .map_err(error::pg)?;
            let ids: Vec<String> = rows
                .iter()
                .map(|row| row.try_get::<_, String>("id"))
                .collect::<Result<_, _>>()
                .map_err(error::pg)?;
            if ids.is_empty() {
                tx.commit().await.map_err(error::pg)?;
                return Ok(Vec::new());
            }
            let nodes = tx
                .query(sql::LIST_READY_NODES, &[&ids, &self.scope()?])
                .await
                .map_err(error::pg)?;
            tx.commit().await.map_err(error::pg)?;

            let mut ready: Vec<ReadyRun> = Vec::new();
            for row in &nodes {
                let run_text: String = row.try_get("run_id").map_err(error::pg)?;
                let node_text: String = row.try_get("node").map_err(error::pg)?;
                let run = RunId::new(&run_text)
                    .map_err(|e| error::corrupt(format!("stored run id: {e}")))?;
                let node = NodeId::new(&node_text)
                    .map_err(|e| error::corrupt(format!("stored node id: {e}")))?;
                match ready.last_mut() {
                    Some(last) if last.run == run => last.ready.push(node),
                    _ => ready.push(ReadyRun {
                        run,
                        ready: vec![node],
                    }),
                }
            }
            Ok(ready)
        })
    }

    fn find_by_key<'a>(&'a self, key: &'a str) -> ExecFuture<'a, Option<Run>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            let row = conn
                .query_opt(sql::SELECT_RUN_BY_KEY, &[&key, &self.scope()?])
                .await
                .map_err(error::pg)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let mut run = convert::run_from_row(&row)?;
            let nodes = conn
                .query(sql::SELECT_NODES, &[&run.id.as_str(), &self.scope()?])
                .await
                .map_err(error::pg)?;
            fill_nodes(&mut run, &nodes)?;
            Ok(Some(run))
        })
    }
}

/// The `Unit` port: a transition on the caller's storage transaction.
///
/// [`Unit::commit_with`] opens one [`PgTransaction`], runs the caller's document work on it,
/// applies the transition through [`PgExecStore::apply_in`] on the same transaction, and
/// commits once. The document and the run state commit or roll back together; a failure in the
/// work, in `apply_in` or at commit rolls both back.
impl Unit<PgTransaction> for PgExecStore {
    fn commit_with<'a>(
        &'a self,
        transition: &'a Transition,
        expected_version: u64,
        work: &'a mut (dyn for<'t> FnMut(&'t mut PgTransaction) -> WorkFuture<'t> + Send),
    ) -> ExecFuture<'a, Run> {
        Box::pin(async move {
            let mut tx = self.begin("dag_unit").await?;
            // On an error the `?` drops `tx`, whose `Drop` rolls the transaction back.
            work(&mut tx).await?;
            let run = self.apply_in(&mut tx, transition, expected_version).await?;
            tx.commit().await.map_err(ExecError::StorageFailed)?;
            Ok(run)
        })
    }
}

/// A factory that opens [`PgExecStore`]s over one shared pool, for the contract suite.
///
/// Each `open` returns a store over the same database, so the suite can drop one store, open
/// another and prove that state survived.
#[derive(Clone)]
pub struct PgExecStoreFactory {
    pool: Pool,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl std::fmt::Debug for PgExecStoreFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgExecStoreFactory").finish_non_exhaustive()
    }
}

impl PgExecStoreFactory {
    /// A factory over `pool` and the storage `backend` on the same database.
    #[must_use]
    pub fn new(pool: Pool, backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            pool,
            backend,
            catalog,
        }
    }
}

impl ExecStoreFactory for PgExecStoreFactory {
    fn open(&self) -> Box<dyn ExecStore> {
        Box::new(PgExecStore::new(
            self.pool.clone(),
            self.backend.clone(),
            Arc::clone(&self.catalog),
        ))
    }
}
