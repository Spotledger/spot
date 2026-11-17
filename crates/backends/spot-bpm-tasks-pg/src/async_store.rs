//! [`AsyncPgTaskStore`]: the PostgreSQL [`AsyncTaskStore`], awaited on the caller's own runtime
//! (WP-BPM-05.T16).
//!
//! [`PgTaskStore`](crate::PgTaskStore) is synchronous and goes through the blocking bridge, which
//! refuses a call from a tokio runtime thread (`pg_bridge.wrong_thread`, ADR-082 addendum 1,
//! A2 rule 1). The inbox is a request-path feature, so this store is the one an HTTP handler
//! uses: it takes a connection from the `deadpool-postgres` pool the sync store already
//! validates and awaits the statement. It does **not** use `spot-pg-bridge`, that being the
//! point, and it never blocks a runtime worker.
//!
//! It reuses what the sync store keeps unchanged: the [`sql`] statements, the [`codec`] row
//! decoding and the [`schema`] tables. So the same tables hold the same rows and the two stores
//! can run side by side on one database: the same one-statement optimistic version (a stale
//! `commit` updates nothing and inserts no event), the same idempotent create per
//! `(case, node)`, the same tenant filter as parameter `$1` of every statement.
//!
//! # Deadlines
//!
//! Every call has one deadline covering the wait for a pooled connection **and** the statement
//! ([`READ_DEADLINE`](crate::READ_DEADLINE) and [`WRITE_DEADLINE`](crate::WRITE_DEADLINE) by
//! default, [`AsyncPgTaskStore::with_deadlines`] to change them). A call that misses it fails
//! with `bpm_tasks.store_failed` naming `deadline`, never a default value. The connection the
//! statement was running on is **closed, not returned to the pool**, so a late server-side
//! statement can never run in front of the next user of that connection; the pool opens a fresh
//! one. A write that timed out may or may not have been applied: read it back before retrying
//! (`create` and the version check make a retry safe).
//!
//! # Security
//!
//! The same as the sync store: tenant from the constructor only; fixed SQL with bound
//! parameters; a backend failure is `StoreFailed` and never an empty result; an error names the
//! store and the SQLSTATE class only, never a row value, form field, SQL or key; a refused write
//! leaves neither a changed task nor an event.

use std::time::Duration;

use deadpool_postgres::{Object, Pool};
use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_tasks::TaskFuture;
use spot_bpm_tasks::{
    AsyncTaskStore, Task, TaskEvent, TaskFilter, TaskId, TasksError, TasksResult,
};
use tokio::time::{Instant, error::Elapsed, timeout_at};

use crate::codec::{event_from_row, task_from_row, whole_seconds};
use crate::error::{SetupError, failed, pg};
use crate::{READ_DEADLINE, WRITE_DEADLINE, schema, sql};

/// The longest a tenant name may be (the sync store's limit).
const MAX_TENANT_LEN: usize = 128;

/// The task inbox in PostgreSQL for a caller on a tokio runtime. Build it with
/// [`AsyncPgTaskStore::new`], create the tables once with [`AsyncPgTaskStore::prepare`] (or the
/// sync store's `prepare`), then use it as an [`AsyncTaskStore`].
///
/// The store is bound to **one tenant** for its whole life, exactly like the sync store.
pub struct AsyncPgTaskStore {
    pool: Pool,
    tenant: String,
    read_deadline: Duration,
    write_deadline: Duration,
}

impl std::fmt::Debug for AsyncPgTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncPgTaskStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl AsyncPgTaskStore {
    /// Builds the store for `tenant` over `pool`, with the default deadlines.
    ///
    /// # Errors
    /// `bpm_tasks_pg.bad_setup` when the tenant is empty, longer than 128 characters or holds a
    /// control character.
    pub fn new(pool: Pool, tenant: &str) -> Result<Self, SetupError> {
        if tenant.is_empty()
            || tenant.len() > MAX_TENANT_LEN
            || tenant.chars().any(char::is_control)
        {
            return Err(SetupError::bad_setup("the tenant name is not usable"));
        }
        Ok(Self {
            pool,
            tenant: tenant.to_owned(),
            read_deadline: READ_DEADLINE,
            write_deadline: WRITE_DEADLINE,
        })
    }

    /// The same store with its own deadlines: one for a read, one for a write.
    ///
    /// # Errors
    /// `bpm_tasks_pg.bad_setup` when either deadline is zero (a call could never succeed).
    pub fn with_deadlines(mut self, read: Duration, write: Duration) -> Result<Self, SetupError> {
        if read.is_zero() || write.is_zero() {
            return Err(SetupError::bad_setup("a deadline must not be zero"));
        }
        self.read_deadline = read;
        self.write_deadline = write;
        Ok(self)
    }

    /// The tenant every row is stamped with and every statement filters by.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables and indexes if missing, in one transaction under the same advisory
    /// lock the sync store uses. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `bpm_tasks_pg.migration_failed` with a fixed sentence.
    pub async fn prepare(&self) -> Result<(), SetupError> {
        let end = Instant::now() + crate::PREPARE_DEADLINE;
        let migration = || SetupError::migration("the task tables could not be created");
        let mut conn = match timeout_at(end, self.pool.get()).await {
            Ok(Ok(conn)) => conn,
            _ => return Err(migration()),
        };
        match timeout_at(end, schema::prepare(&mut conn)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(migration()),
            Err(_) => {
                drop(Object::take(conn));
                Err(migration())
            }
        }
    }

    /// The tasks (any status) that hold any of `roles`, sorted by id: the by-role inbox view,
    /// served by `spot_bpm_task_roles_idx`. No roles gives no tasks.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails or a stored row is corrupt.
    pub async fn list_by_role(&self, roles: &[String]) -> TasksResult<Vec<Task>> {
        let end = Instant::now() + self.read_deadline;
        let mut conn = self.acquire(end).await?;
        let out = timeout_at(end, list_by_role_op(&mut conn, &self.tenant, roles)).await;
        settle(conn, out)
    }

    /// A pooled connection before `end`.
    async fn acquire(&self, end: Instant) -> TasksResult<Object> {
        match timeout_at(end, self.pool.get()).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(_)) => Err(failed("pool: no database connection is available")),
            Err(_) => Err(failed(
                "deadline: no connection became free before the call's deadline",
            )),
        }
    }
}

/// The result of a statement that ran under a deadline. A statement that missed it takes its
/// connection with it: the connection is closed, never returned to the pool.
fn settle<T>(conn: Object, out: Result<TasksResult<T>, Elapsed>) -> TasksResult<T> {
    match out {
        Ok(result) => result,
        Err(_) => {
            drop(Object::take(conn));
            Err(failed(
                "deadline: the call missed its deadline; a write may or may not have applied",
            ))
        }
    }
}

/// The task's facts as the bound parameters of an insert or update (the sync store's `Row`).
struct Row {
    assignees: Json,
    roles: Json,
    delegation: Option<Json>,
    due: Option<OffsetDateTime>,
    sla: Option<Json>,
    body: Json,
}

impl Row {
    fn of(task: &Task) -> TasksResult<Row> {
        macro_rules! json {
            ($value:expr) => {
                serde_json::to_value($value).map_err(|_| failed("the task cannot be encoded"))?
            };
        }
        Ok(Row {
            assignees: json!(&task.assignees),
            roles: json!(&task.roles),
            delegation: match &task.delegation {
                Some(d) => Some(json!(d)),
                None => None,
            },
            due: task.due.map(whole_seconds).transpose()?,
            sla: match &task.sla {
                Some(s) => Some(json!(s)),
                None => None,
            },
            body: json!(task),
        })
    }
}

/// The event as the bound parameters of an insert: action, actor, instant and body.
fn event_parts(event: &TaskEvent, seq: u64) -> TasksResult<(String, String, OffsetDateTime, Json)> {
    let mut stamped = event.clone();
    stamped.seq = seq;
    let body = serde_json::to_value(&stamped).map_err(|_| failed("the event cannot be encoded"))?;
    Ok((
        stamped.action.as_str().to_owned(),
        stamped.actor,
        whole_seconds(stamped.at)?,
        body,
    ))
}

async fn list_by_role_op(
    conn: &mut Object,
    tenant: &str,
    roles: &[String],
) -> TasksResult<Vec<Task>> {
    let tenant = tenant.to_owned();
    let roles = roles.to_vec();
    let rows = conn
        .query(sql::LIST_BY_ROLE, &[&tenant, &roles])
        .await
        .map_err(|e| pg(&e))?;
    rows.iter().map(task_from_row).collect()
}

/// Everything `create` binds, encoded before a connection is taken.
struct CreateInput {
    stored: Task,
    row: Row,
    action: String,
    actor: String,
    at: OffsetDateTime,
    event_body: Json,
    created_at: OffsetDateTime,
}

async fn create_op(conn: &mut Object, tenant: &str, input: CreateInput) -> TasksResult<Task> {
    let CreateInput {
        stored,
        row,
        action,
        actor,
        at,
        event_body,
        created_at,
    } = input;
    let tenant = tenant.to_owned();
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let id = stored.id.as_str().to_owned();
    let status = stored.status.as_str().to_owned();
    let inserted = tx
        .query_opt(
            sql::INSERT_TASK,
            &[
                &tenant,
                &id,
                &stored.case,
                &stored.node,
                &stored.process,
                &status,
                &row.assignees,
                &row.roles,
                &row.delegation,
                &row.due,
                &row.sla,
                &created_at,
                &row.body,
            ],
        )
        .await
        .map_err(|e| pg(&e))?;
    if inserted.is_some() {
        tx.execute(
            sql::INSERT_FIRST_EVENT,
            &[&tenant, &id, &action, &actor, &at, &event_body],
        )
        .await
        .map_err(|e| pg(&e))?;
        tx.commit().await.map_err(|e| pg(&e))?;
        return Ok(stored);
    }
    // Nothing inserted: the pair is already taken (a replay), or the task id is.
    let existing = tx
        .query_opt(sql::FIND_BY_NODE, &[&tenant, &stored.case, &stored.node])
        .await
        .map_err(|e| pg(&e))?;
    match existing {
        Some(found) => task_from_row(&found),
        None => Err(failed("the task id is already used by another node")),
    }
}

async fn load_op(conn: &mut Object, tenant: &str, id: &TaskId) -> TasksResult<Option<Task>> {
    let tenant = tenant.to_owned();
    let id = id.as_str().to_owned();
    conn.query_opt(sql::LOAD, &[&tenant, &id])
        .await
        .map_err(|e| pg(&e))?
        .as_ref()
        .map(task_from_row)
        .transpose()
}

async fn find_by_node_op(
    conn: &mut Object,
    tenant: &str,
    case: &str,
    node: &str,
) -> TasksResult<Option<Task>> {
    let tenant = tenant.to_owned();
    let (case, node) = (case.to_owned(), node.to_owned());
    conn.query_opt(sql::FIND_BY_NODE, &[&tenant, &case, &node])
        .await
        .map_err(|e| pg(&e))?
        .as_ref()
        .map(task_from_row)
        .transpose()
}

/// Everything `commit` binds, encoded before a connection is taken.
struct CommitInput {
    stored: Task,
    row: Row,
    has_event: bool,
    action: String,
    actor: String,
    at: OffsetDateTime,
    event_body: Json,
    expected_version: u64,
    /// `None` when the expected version does not fit a bigint (it cannot match a stored row).
    expected: Option<i64>,
}

async fn commit_op(
    conn: &mut Object,
    tenant: &str,
    id: &TaskId,
    input: CommitInput,
) -> TasksResult<Task> {
    let CommitInput {
        stored,
        row,
        has_event,
        action,
        actor,
        at,
        event_body,
        expected_version,
        expected,
    } = input;
    let tenant = tenant.to_owned();
    let task_id = id.as_str().to_owned();
    let status = stored.status.as_str().to_owned();
    if let Some(expected) = expected {
        let result = conn
            .query_one(
                sql::COMMIT,
                &[
                    &tenant,
                    &task_id,
                    &expected,
                    &stored.case,
                    &stored.node,
                    &stored.process,
                    &status,
                    &row.assignees,
                    &row.roles,
                    &row.delegation,
                    &row.due,
                    &row.sla,
                    &row.body,
                    &has_event,
                    &action,
                    &actor,
                    &at,
                    &event_body,
                ],
            )
            .await
            .map_err(|e| pg(&e))?;
        let updated: i64 = result.try_get(0).map_err(|e| pg(&e))?;
        if updated == 1 {
            return Ok(stored);
        }
    }
    // Nothing was written. Say why, from the stored row.
    let found = conn
        .query_opt(sql::DIAGNOSE, &[&tenant, &task_id])
        .await
        .map_err(|e| pg(&e))?;
    let Some(found) = found else {
        return Err(TasksError::TaskNotFound { id: task_id });
    };
    let current: i64 = found.try_get("version").map_err(|e| pg(&e))?;
    let current = u64::try_from(current).map_err(|_| failed("a stored version is negative"))?;
    if current != expected_version {
        return Err(TasksError::VersionConflict {
            id: task_id,
            current,
            expected: expected_version,
        });
    }
    Err(failed("a commit may not change the case or node of a task"))
}

async fn events_op(conn: &mut Object, tenant: &str, id: &TaskId) -> TasksResult<Vec<TaskEvent>> {
    let tenant = tenant.to_owned();
    let id = id.as_str().to_owned();
    let rows = conn
        .query(sql::EVENTS, &[&tenant, &id])
        .await
        .map_err(|e| pg(&e))?;
    rows.iter().map(event_from_row).collect()
}

async fn list_op(conn: &mut Object, tenant: &str, filter: &TaskFilter) -> TasksResult<Vec<Task>> {
    let tenant = tenant.to_owned();
    let status: Option<Vec<String>> = filter
        .status
        .as_ref()
        .map(|all| all.iter().map(|s| s.as_str().to_owned()).collect());
    let case = filter.case.clone();
    let process = filter.process.clone();
    let assignees = filter.assignees.clone();
    let due_before = filter.due_before;
    let rows = conn
        .query(
            sql::LIST,
            &[&tenant, &status, &case, &process, &assignees, &due_before],
        )
        .await
        .map_err(|e| pg(&e))?;
    let mut tasks = Vec::with_capacity(rows.len());
    for row in &rows {
        let task = task_from_row(row)?;
        // The same predicate the reference store applies, so the stores cannot drift.
        if filter.matches(&task) {
            tasks.push(task);
        }
    }
    Ok(tasks)
}

impl AsyncTaskStore for AsyncPgTaskStore {
    fn create(&self, task: Task, event: TaskEvent) -> TaskFuture<'_, Task> {
        Box::pin(async move {
            if event.task != task.id {
                return Err(failed("the created event belongs to another task"));
            }
            let mut stored = task;
            stored.version = 1;
            let row = Row::of(&stored)?;
            let (action, actor, at, event_body) = event_parts(&event, 1)?;
            let created_at = whole_seconds(stored.created_at)?;
            let input = CreateInput {
                stored,
                row,
                action,
                actor,
                at,
                event_body,
                created_at,
            };
            let end = Instant::now() + self.write_deadline;
            let mut conn = self.acquire(end).await?;
            let out = timeout_at(end, create_op(&mut conn, &self.tenant, input)).await;
            settle(conn, out)
        })
    }

    fn load<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>> {
        Box::pin(async move {
            let end = Instant::now() + self.read_deadline;
            let mut conn = self.acquire(end).await?;
            let out = timeout_at(end, load_op(&mut conn, &self.tenant, id)).await;
            settle(conn, out)
        })
    }

    fn find_by_node<'a>(&'a self, case: &'a str, node: &'a str) -> TaskFuture<'a, Option<Task>> {
        Box::pin(async move {
            let end = Instant::now() + self.read_deadline;
            let mut conn = self.acquire(end).await?;
            let out = timeout_at(end, find_by_node_op(&mut conn, &self.tenant, case, node)).await;
            settle(conn, out)
        })
    }

    fn commit<'a>(
        &'a self,
        id: &'a TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TaskFuture<'a, Task> {
        Box::pin(async move {
            if task.id != *id {
                return Err(failed("the committed task has another id"));
            }
            if event.as_ref().is_some_and(|e| e.task != *id) {
                return Err(failed("the committed event belongs to another task"));
            }
            let mut stored = task;
            stored.version = expected_version.saturating_add(1);
            let row = Row::of(&stored)?;
            // The sequence is stamped by the statement from the task's own counter; this value
            // is a placeholder the statement overwrites.
            let (has_event, action, actor, at, event_body) = match &event {
                Some(event) => {
                    let (action, actor, at, body) = event_parts(event, 1)?;
                    (true, action, actor, at, body)
                }
                None => (
                    false,
                    String::new(),
                    String::new(),
                    whole_seconds(stored.created_at)?,
                    Json::Object(serde_json::Map::new()),
                ),
            };
            let input = CommitInput {
                stored,
                row,
                has_event,
                action,
                actor,
                at,
                event_body,
                expected_version,
                expected: i64::try_from(expected_version).ok(),
            };
            let end = Instant::now() + self.write_deadline;
            let mut conn = self.acquire(end).await?;
            let out = timeout_at(end, commit_op(&mut conn, &self.tenant, id, input)).await;
            settle(conn, out)
        })
    }

    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>> {
        Box::pin(async move {
            let end = Instant::now() + self.read_deadline;
            let mut conn = self.acquire(end).await?;
            let out = timeout_at(end, events_op(&mut conn, &self.tenant, id)).await;
            settle(conn, out)
        })
    }

    fn list<'a>(&'a self, filter: &'a TaskFilter) -> TaskFuture<'a, Vec<Task>> {
        Box::pin(async move {
            let end = Instant::now() + self.read_deadline;
            let mut conn = self.acquire(end).await?;
            let out = timeout_at(end, list_op(&mut conn, &self.tenant, filter)).await;
            settle(conn, out)
        })
    }
}
