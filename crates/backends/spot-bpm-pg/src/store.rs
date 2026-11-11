//! [`PgCaseStore`]: the PostgreSQL [`CaseStore`], over `spot_bpm_case`, `spot_bpm_case_var` and
//! `spot_bpm_node_outcome`.

use std::collections::BTreeMap;

use deadpool_postgres::{Object, Pool};
use serde_json::Value as Json;
use spot_app_api::AppResult;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{
    Attempt, AttemptState, CaseFuture, CaseId, CaseRecord, CaseStore, CaseStoreFactory, CaseUpdate,
    ChildLink, EngineError, EngineResult, TimerRecord,
};
use spot_dag::NodeId;
use spot_dag_exec::Run;
use tokio_postgres::{Client, Row, Transaction};

use crate::convert;
use crate::error;
use crate::schema;
use crate::sql;
use crate::state;

/// The longest a caller-supplied business key may be (the DAG layer's rule).
const MAX_KEY_LEN: usize = 256;

/// The tenant a store stamps its rows with until [`PgCaseStore::with_tenant`] changes it.
pub const DEFAULT_TENANT: &str = "default";

/// The BPM Case state in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgCaseStore::prepare`] once at
/// start-up; a store shares its pool with every clone.
///
/// The store is **tenant-scoped**: every row carries the tenant and every query filters by it,
/// so one database can hold several tenants' Cases. A pool is already per-tenant in production
/// (`spot-postgres`); the tenant key is what keeps the tables honest when they are not.
#[derive(Clone)]
pub struct PgCaseStore {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgCaseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCaseStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgCaseStore {
    /// A store on `pool` for the [`DEFAULT_TENANT`]. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            tenant: DEFAULT_TENANT.to_owned(),
        }
    }

    /// The same store for another tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// The tenant every row is stamped with and every query filters by.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables and indexes if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `bpm_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> EngineResult<Object> {
        self.pool.get().await.map_err(error::pool)
    }

    /// The nodes waiting on one event key: the "Cases waiting on event X" lookup, served by
    /// `spot_bpm_node_outcome_event_idx`.
    ///
    /// # Errors
    /// A store failure.
    pub async fn waiting_on_event(&self, event: &str) -> EngineResult<Vec<(CaseId, NodeId)>> {
        let conn = self.conn().await?;
        let rows = conn
            .query(sql::WAITING_ON_EVENT, &[&self.tenant, &event])
            .await
            .map_err(error::pg)?;
        rows.iter().map(pair_from_row).collect()
    }

    /// The nodes whose timer is due at or before `now`: the "timers due" lookup, served by
    /// `spot_bpm_node_outcome_due_idx`.
    ///
    /// # Errors
    /// A store failure.
    pub async fn due_timers(&self, now: OffsetDateTime) -> EngineResult<Vec<(CaseId, NodeId)>> {
        let conn = self.conn().await?;
        let rows = conn
            .query(sql::DUE_TIMERS, &[&now, &self.tenant])
            .await
            .map_err(error::pg)?;
        rows.iter().map(pair_from_row).collect()
    }
}

/// A `(case, node)` pair from a two-column row.
fn pair_from_row(row: &Row) -> EngineResult<(CaseId, NodeId)> {
    let case: String = row
        .try_get("case_id")
        .map_err(|error| error::corrupt(error.to_string()))?;
    let case =
        CaseId::new(&case).map_err(|error| error::corrupt(format!("stored case id: {error}")))?;
    let node: String = row
        .try_get("node")
        .map_err(|error| error::corrupt(error.to_string()))?;
    let node =
        NodeId::new(&node).map_err(|error| error::corrupt(format!("stored node id: {error}")))?;
    Ok((case, node))
}

/// One node's durable auxiliary state: the engine attempt and, when it is waiting, the timer.
#[derive(Debug, Default)]
struct NodeAux {
    attempt: Option<Attempt>,
    timer: Option<TimerRecord>,
}

/// Whether an attempt state is terminal, so a later non-terminal write never regresses it.
fn is_terminal(state: AttemptState) -> bool {
    matches!(state, AttemptState::Done | AttemptState::Failed)
}

/// The node rows of one Case, keyed by node.
fn node_aux_map(rows: &[Row]) -> EngineResult<BTreeMap<NodeId, NodeAux>> {
    let mut map: BTreeMap<NodeId, NodeAux> = BTreeMap::new();
    for row in rows {
        let attempt = convert::attempt_from_row(row)?;
        let node = attempt.node.clone();
        let timer = convert::timer_from_row(row)?;
        map.insert(
            node,
            NodeAux {
                attempt: Some(attempt),
                timer,
            },
        );
    }
    Ok(map)
}

/// Merges a whole [`CaseUpdate`] into the node map, returning the nodes whose row changed.
///
/// The exactly-once guard lives here: a stored **terminal** outcome for the same
/// `(case, node, attempt)` is never overwritten, so a replayed commit returns the stored outcome
/// instead of storing a second one.
fn merge_nodes(map: &mut BTreeMap<NodeId, NodeAux>, update: &CaseUpdate) -> Vec<NodeId> {
    let mut touched = Vec::new();
    if let Some(attempt) = &update.attempt {
        let entry = map.entry(attempt.node.clone()).or_default();
        let replace = entry
            .attempt
            .as_ref()
            .is_none_or(|stored| attempt.attempt >= stored.attempt);
        let guarded = entry
            .attempt
            .as_ref()
            .is_some_and(|stored| stored.attempt == attempt.attempt && is_terminal(stored.state));
        if replace && !guarded {
            entry.attempt = Some(attempt.clone());
        }
        touched.push(attempt.node.clone());
    }
    if let Some(timer) = &update.timer {
        let entry = map.entry(timer.node.clone()).or_default();
        let replace = entry
            .timer
            .as_ref()
            .is_none_or(|stored| timer.attempt >= stored.attempt);
        if replace {
            entry.timer = Some(timer.clone());
        }
        touched.push(timer.node.clone());
    }
    if let Some(node) = &update.clear_timer {
        if let Some(entry) = map.get_mut(node) {
            entry.timer = None;
        }
        touched.push(node.clone());
    }
    touched.sort();
    touched.dedup();
    touched
}

/// The Case record assembled from its parts.
fn assemble(
    run: Run,
    variables: Json,
    aux: BTreeMap<NodeId, NodeAux>,
    children: BTreeMap<NodeId, ChildLink>,
    initiator: Option<String>,
) -> CaseRecord {
    let mut attempts = BTreeMap::new();
    let mut timers = BTreeMap::new();
    for (node, entry) in aux {
        if let Some(attempt) = entry.attempt {
            attempts.insert(node.clone(), attempt);
        }
        if let Some(timer) = entry.timer {
            timers.insert(node, timer);
        }
    }
    CaseRecord {
        run,
        variables,
        attempts,
        timers,
        children,
        initiator,
    }
}

/// A version that fits the `bigint` column.
fn version_to_db(version: u64) -> EngineResult<i64> {
    i64::try_from(version).map_err(|_| error::corrupt("case version is out of range"))
}

/// An attempt number that fits the `integer` column.
fn attempt_to_db(attempt: u32) -> i32 {
    i32::try_from(attempt).unwrap_or(i32::MAX)
}

/// The `children` map of a `spot_bpm_case` row.
fn children_of(row: &Row) -> EngineResult<BTreeMap<NodeId, ChildLink>> {
    let children: Json = row
        .try_get("children")
        .map_err(|error| error::corrupt(error.to_string()))?;
    convert::children_from_json(&children)
}

/// The variables of one Case, or an empty object when the row is missing.
async fn variables_from(conn: &Client, id: &CaseId) -> EngineResult<Json> {
    let row = conn
        .query_opt(sql::SELECT_VAR, &[&id.as_str()])
        .await
        .map_err(error::pg)?;
    variables_of(row)
}

/// The variables of one Case read through an open transaction.
async fn variables_from_tx(tx: &Transaction<'_>, id: &CaseId) -> EngineResult<Json> {
    let row = tx
        .query_opt(sql::SELECT_VAR, &[&id.as_str()])
        .await
        .map_err(error::pg)?;
    variables_of(row)
}

/// Parses a `spot_bpm_case_var` row, defaulting to an empty object.
fn variables_of(row: Option<Row>) -> EngineResult<Json> {
    match row {
        Some(row) => row
            .try_get("variables")
            .map_err(|error| error::corrupt(error.to_string())),
        None => Ok(Json::Object(serde_json::Map::new())),
    }
}

/// Assembles a Case from a `spot_bpm_case` row, reading its nodes and variables.
async fn assemble_from(conn: &Client, row: &Row) -> EngineResult<CaseRecord> {
    let run = convert::run_from_row(row)?;
    let initiator = convert::initiator_from_row(row)?;
    let children = children_of(row)?;
    let node_rows = conn
        .query(sql::SELECT_NODES, &[&run.id.as_str()])
        .await
        .map_err(error::pg)?;
    let aux = node_aux_map(&node_rows)?;
    let variables = variables_from(conn, &run.id).await?;
    Ok(assemble(run, variables, aux, children, initiator))
}

/// Writes one node's merged row, replacing whatever rows the node had.
async fn write_node(
    tx: &Transaction<'_>,
    tenant: &str,
    case: &CaseId,
    node: &NodeId,
    entry: &NodeAux,
) -> EngineResult<()> {
    tx.execute(sql::DELETE_NODE, &[&case.as_str(), &node.as_str()])
        .await
        .map_err(error::pg)?;
    let Some(attempt) = &entry.attempt else {
        // The engine always stores a wait with the attempt that registered it, so a row with a
        // timer but no attempt is a caller invariant violation, not something to drop silently.
        if entry.timer.is_some() {
            return Err(error::store_failed(
                "a node wait must be stored with the attempt that registered it",
            ));
        }
        return Ok(());
    };
    let state_text = convert::attempt_state_text(attempt.state);
    let attempt_number = attempt_to_db(attempt.attempt);
    let started_at = convert::at(attempt.started_at)?;
    let not_before = attempt.not_before.map(convert::at).transpose()?;
    let branches = serde_json::to_value(&attempt.branches)
        .map_err(|error| error::corrupt(format!("branches: {error}")))?;
    let (due, event, human) = match &entry.timer {
        Some(timer) => (
            timer.due.map(convert::at).transpose()?,
            timer.event.clone(),
            timer.human.clone(),
        ),
        None => (None, None, None),
    };
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 14] = [
        &case.as_str(),
        &tenant,
        &node.as_str(),
        &attempt_number,
        &state_text,
        &attempt.output,
        &branches,
        &attempt.failure,
        &started_at,
        &not_before,
        &attempt.is_retry,
        &due,
        &event,
        &human,
    ];
    tx.execute(sql::INSERT_NODE, &params)
        .await
        .map_err(error::pg)?;
    Ok(())
}

/// Maps a failed `spot_bpm_case` insert: a unique violation is the case id or the business key.
fn map_insert(error: tokio_postgres::Error) -> EngineError {
    if error::is_unique_violation(&error) {
        return error::store_failed("a case with this id or business key already exists");
    }
    error::pg(error)
}

impl CaseStore for PgCaseStore {
    fn create<'a>(&'a self, record: CaseRecord) -> CaseFuture<'a, CaseRecord> {
        Box::pin(async move {
            if record.run.key.is_empty() || record.run.key.len() > MAX_KEY_LEN {
                return Err(error::store_failed(
                    "the business key must be 1 to 256 characters",
                ));
            }
            let run = record.run.clone();
            let children = record.children.clone();
            let nodes = convert::nodes_to_json(&run.nodes)?;
            let taken = convert::taken_to_json(&run.taken)?;
            let children_json = convert::children_to_json(&children)?;
            let started_at = convert::at(run.started_at)?;
            let ended_at = run.ended_at.map(convert::at).transpose()?;
            let version = version_to_db(run.version)?;
            let dag_version = i32::try_from(run.dag_version).unwrap_or(i32::MAX);
            let status = convert::run_status_text(run.status);
            let has_ready = !run.ready_nodes().is_empty();
            let has_children = !children.is_empty();

            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(error::pg)?;
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 16] = [
                &run.id.as_str(),
                &self.tenant,
                &run.key,
                &run.dag_key,
                &dag_version,
                &run.content_hash,
                &status,
                &version,
                &started_at,
                &ended_at,
                &record.initiator,
                &nodes,
                &taken,
                &children_json,
                &has_ready,
                &has_children,
            ];
            tx.execute(sql::INSERT_CASE, &params)
                .await
                .map_err(map_insert)?;
            tx.execute(
                sql::UPSERT_VAR,
                &[&run.id.as_str(), &self.tenant, &record.variables],
            )
            .await
            .map_err(error::pg)?;
            // The run's own children column carries the links; only attempts get a node row.
            for (node, attempt) in &record.attempts {
                let entry = NodeAux {
                    attempt: Some(attempt.clone()),
                    timer: record.timers.get(node).cloned(),
                };
                write_node(&tx, &self.tenant, &run.id, node, &entry).await?;
            }
            tx.commit().await.map_err(error::pg)?;
            Ok(record)
        })
    }

    fn load<'a>(&'a self, id: &'a CaseId) -> CaseFuture<'a, Option<CaseRecord>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            let row = conn
                .query_opt(sql::SELECT_CASE, &[&id.as_str(), &self.tenant])
                .await
                .map_err(error::pg)?;
            match row {
                Some(row) => Ok(Some(assemble_from(&conn, &row).await?)),
                None => Ok(None),
            }
        })
    }

    fn commit<'a>(
        &'a self,
        id: &'a CaseId,
        expected_version: u64,
        update: CaseUpdate,
    ) -> CaseFuture<'a, CaseRecord> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(error::pg)?;
            let row = tx
                .query_opt(sql::SELECT_CASE_FOR_UPDATE, &[&id.as_str(), &self.tenant])
                .await
                .map_err(error::pg)?;
            let Some(row) = row else {
                return Err(EngineError::CaseNotFound { id: id.to_string() });
            };
            let mut run = convert::run_from_row(&row)?;
            let initiator = convert::initiator_from_row(&row)?;
            let mut children = children_of(&row)?;
            let node_rows = tx
                .query(sql::SELECT_NODES, &[&id.as_str()])
                .await
                .map_err(error::pg)?;
            let mut aux = node_aux_map(&node_rows)?;

            // The idempotency key wins over the version check: a retried transition is a no-op
            // (but its other parts — an attempt, variables — still merge, as the reference does).
            let already = update
                .transition
                .as_ref()
                .is_some_and(|transition| state::already_applied(&run, &transition.step));
            if !already {
                if run.version != expected_version {
                    return Err(EngineError::VersionConflict {
                        current: run.version,
                        expected: expected_version,
                    });
                }
                if let Some(transition) = &update.transition {
                    state::apply_step(&mut run, &transition.step)?;
                    run.version += 1;
                }
            }
            let touched = merge_nodes(&mut aux, &update);
            if let Some(child) = &update.child {
                children.insert(child.node.clone(), child.clone());
            }

            let nodes = convert::nodes_to_json(&run.nodes)?;
            let taken = convert::taken_to_json(&run.taken)?;
            let children_json = convert::children_to_json(&children)?;
            let ended_at = run.ended_at.map(convert::at).transpose()?;
            let version = version_to_db(run.version)?;
            let status = convert::run_status_text(run.status);
            let has_ready = !run.ready_nodes().is_empty();
            let has_children = !children.is_empty();
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 10] = [
                &id.as_str(),
                &self.tenant,
                &status,
                &ended_at,
                &version,
                &nodes,
                &taken,
                &children_json,
                &has_ready,
                &has_children,
            ];
            tx.execute(sql::UPDATE_CASE, &params)
                .await
                .map_err(error::pg)?;
            for node in &touched {
                if let Some(entry) = aux.get(node) {
                    write_node(&tx, &self.tenant, id, node, entry).await?;
                }
            }
            let variables = match &update.variables {
                Some(variables) => {
                    tx.execute(sql::UPSERT_VAR, &[&id.as_str(), &self.tenant, variables])
                        .await
                        .map_err(error::pg)?;
                    variables.clone()
                }
                None => variables_from_tx(&tx, id).await?,
            };
            tx.commit().await.map_err(error::pg)?;
            Ok(assemble(run, variables, aux, children, initiator))
        })
    }

    fn list_runnable<'a>(&'a self, now: OffsetDateTime, limit: u32) -> CaseFuture<'a, Vec<CaseId>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            let rows = conn
                .query(sql::LIST_RUNNABLE, &[&self.tenant, &now, &i64::from(limit)])
                .await
                .map_err(error::pg)?;
            let mut ids = Vec::with_capacity(rows.len());
            for row in &rows {
                let text: String = row
                    .try_get("case_id")
                    .map_err(|error| error::corrupt(error.to_string()))?;
                let id = CaseId::new(&text)
                    .map_err(|error| error::corrupt(format!("stored case id: {error}")))?;
                ids.push(id);
            }
            Ok(ids)
        })
    }

    fn find_by_key<'a>(&'a self, key: &'a str) -> CaseFuture<'a, Option<CaseRecord>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            let row = conn
                .query_opt(sql::SELECT_CASE_BY_KEY, &[&self.tenant, &key])
                .await
                .map_err(error::pg)?;
            match row {
                Some(row) => Ok(Some(assemble_from(&conn, &row).await?)),
                None => Ok(None),
            }
        })
    }
}

/// A factory that opens [`PgCaseStore`]s over one shared pool, for the engine's store factory.
#[derive(Clone)]
pub struct PgCaseStoreFactory {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgCaseStoreFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCaseStoreFactory")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgCaseStoreFactory {
    /// A factory over `pool` for the [`DEFAULT_TENANT`].
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            tenant: DEFAULT_TENANT.to_owned(),
        }
    }

    /// The same factory for another tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Creates the tables and indexes if missing.
    ///
    /// # Errors
    /// `bpm_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }
}

impl CaseStoreFactory for PgCaseStoreFactory {
    fn open(&self) -> Box<dyn CaseStore> {
        Box::new(PgCaseStore {
            pool: self.pool.clone(),
            tenant: self.tenant.clone(),
        })
    }
}
