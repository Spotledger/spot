//! [`PgTaskStore`]: the PostgreSQL `TaskStore`, through the shared bridge.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_tasks::{Task, TaskEvent, TaskFilter, TaskId, TaskStore, TasksError, TasksResult};
use spot_pg_bridge::{BoxFuture, Bridge, PooledConn};

use crate::codec::{event_from_row, task_from_row, whole_seconds};
use crate::error::{SetupError, failed, from_bridge, pg};
use crate::{schema, sql};

/// Deadline of a read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a write (one statement or one short transaction).
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The longest a tenant name may be.
const MAX_TENANT_LEN: usize = 128;

/// The task inbox in PostgreSQL. Build it with [`PgTaskStore::new`], create the tables once with
/// [`PgTaskStore::prepare`], then give it to `TaskService` as the `TaskStore`.
///
/// The store is bound to **one tenant** for its whole life: the tenant is stamped on every row
/// and filtered on in every statement, and no method takes one.
///
/// **Call it from a plain thread, never from inside a tokio runtime** (see the crate docs): a
/// call from a runtime thread is refused with `bpm_tasks.store_failed`, never run.
pub struct PgTaskStore {
    bridge: Arc<Bridge>,
    tenant: Arc<str>,
}

impl std::fmt::Debug for PgTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgTaskStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgTaskStore {
    /// Builds the store for `tenant` on the process's shared `bridge` over `pool`.
    ///
    /// `pool` is the pool the bridge was built on; the bridge configuration is validated against
    /// its size again here.
    ///
    /// # Errors
    /// `bpm_tasks_pg.bad_setup` when the tenant is empty, longer than 128 characters or holds a
    /// control character, or when the bridge configuration cannot hold against the pool.
    pub fn new(
        bridge: Arc<Bridge>,
        pool: &Pool,
        tenant: &str,
    ) -> std::result::Result<Self, SetupError> {
        if tenant.is_empty()
            || tenant.len() > MAX_TENANT_LEN
            || tenant.chars().any(char::is_control)
        {
            return Err(SetupError::bad_setup("the tenant name is not usable"));
        }
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                SetupError::bad_setup("the bridge configuration does not hold against the pool")
            })?;
        Ok(Self {
            bridge,
            tenant: Arc::from(tenant),
        })
    }

    /// The tenant every row is stamped with and every statement filters by.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables and indexes if missing, in one transaction under an advisory lock.
    /// Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `bpm_tasks_pg.migration_failed` with a fixed sentence.
    pub fn prepare(&self) -> std::result::Result<(), SetupError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move { schema::prepare(&mut conn).await.map_err(|_| ()) })
            })
            .map_err(|_| SetupError::migration("the task tables could not be created"))
    }

    /// The tasks (any status) that hold any of `roles`, sorted by id: the by-role inbox view,
    /// served by `spot_bpm_task_roles_idx`. No roles gives no tasks.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails or a stored row is corrupt.
    pub fn list_by_role(&self, roles: &[String]) -> TasksResult<Vec<Task>> {
        let tenant = self.tenant.to_string();
        let roles = roles.to_vec();
        self.read(move |conn| {
            Box::pin(async move {
                let rows = conn
                    .query(sql::LIST_BY_ROLE, &[&tenant, &roles])
                    .await
                    .map_err(|e| pg(&e))?;
                rows.iter().map(task_from_row).collect()
            })
        })
    }

    fn read<T, F>(&self, op: F) -> TasksResult<T>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, TasksResult<T>> + Send + 'static,
    {
        from_bridge(self.bridge.run(READ_DEADLINE, op))
    }

    fn write<T, F>(&self, op: F) -> TasksResult<T>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, TasksResult<T>> + Send + 'static,
    {
        from_bridge(self.bridge.run(WRITE_DEADLINE, op))
    }
}

/// The task's facts as the bound parameters of an insert or update.
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

impl TaskStore for PgTaskStore {
    fn create(&self, task: Task, event: TaskEvent) -> TasksResult<Task> {
        if event.task != task.id {
            return Err(failed("the created event belongs to another task"));
        }
        let mut stored = task;
        stored.version = 1;
        let row = Row::of(&stored)?;
        let (action, actor, at, event_body) = event_parts(&event, 1)?;
        let created_at = whole_seconds(stored.created_at)?;
        let tenant = self.tenant.to_string();
        self.write(move |mut conn| {
            Box::pin(async move {
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
            })
        })
    }

    fn load(&self, id: &TaskId) -> TasksResult<Option<Task>> {
        let tenant = self.tenant.to_string();
        let id = id.as_str().to_owned();
        self.read(move |conn| {
            Box::pin(async move {
                conn.query_opt(sql::LOAD, &[&tenant, &id])
                    .await
                    .map_err(|e| pg(&e))?
                    .as_ref()
                    .map(task_from_row)
                    .transpose()
            })
        })
    }

    fn find_by_node(&self, case: &str, node: &str) -> TasksResult<Option<Task>> {
        let tenant = self.tenant.to_string();
        let (case, node) = (case.to_owned(), node.to_owned());
        self.read(move |conn| {
            Box::pin(async move {
                conn.query_opt(sql::FIND_BY_NODE, &[&tenant, &case, &node])
                    .await
                    .map_err(|e| pg(&e))?
                    .as_ref()
                    .map(task_from_row)
                    .transpose()
            })
        })
    }

    fn commit(
        &self,
        id: &TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TasksResult<Task> {
        if task.id != *id {
            return Err(failed("the committed task has another id"));
        }
        if event.as_ref().is_some_and(|e| e.task != *id) {
            return Err(failed("the committed event belongs to another task"));
        }
        let mut stored = task;
        stored.version = expected_version.saturating_add(1);
        let row = Row::of(&stored)?;
        // The sequence is stamped by the statement from the task's own counter; this value is
        // a placeholder the statement overwrites.
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
        // A version beyond bigint cannot match a stored row; the diagnosis below reports it.
        let expected = i64::try_from(expected_version).ok();
        let tenant = self.tenant.to_string();
        let id = id.clone();
        self.write(move |conn| {
            Box::pin(async move {
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
                let current =
                    u64::try_from(current).map_err(|_| failed("a stored version is negative"))?;
                if current != expected_version {
                    return Err(TasksError::VersionConflict {
                        id: task_id,
                        current,
                        expected: expected_version,
                    });
                }
                Err(failed("a commit may not change the case or node of a task"))
            })
        })
    }

    fn events(&self, id: &TaskId) -> TasksResult<Vec<TaskEvent>> {
        let tenant = self.tenant.to_string();
        let id = id.as_str().to_owned();
        self.read(move |conn| {
            Box::pin(async move {
                let rows = conn
                    .query(sql::EVENTS, &[&tenant, &id])
                    .await
                    .map_err(|e| pg(&e))?;
                rows.iter().map(event_from_row).collect()
            })
        })
    }

    fn list(&self, filter: &TaskFilter) -> TasksResult<Vec<Task>> {
        let tenant = self.tenant.to_string();
        let status: Option<Vec<String>> = filter
            .status
            .as_ref()
            .map(|all| all.iter().map(|s| s.as_str().to_owned()).collect());
        let case = filter.case.clone();
        let process = filter.process.clone();
        let assignees = filter.assignees.clone();
        let due_before = filter.due_before;
        let filter = filter.clone();
        self.read(move |conn| {
            Box::pin(async move {
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
                    // The same predicate the reference store applies, so the two cannot drift.
                    if filter.matches(&task) {
                        tasks.push(task);
                    }
                }
                Ok(tasks)
            })
        })
    }
}
