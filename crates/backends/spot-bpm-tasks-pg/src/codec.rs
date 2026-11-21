//! Task and event rows to and from the domain types.
//!
//! A read decodes `body` and then checks it against the columns the row also carries; a row
//! that disagrees with itself is a corrupt row and an error, never skipped and never repaired.

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_tasks::{Task, TaskEvent, TaskId, TasksError};
use tokio_postgres::Row;

use crate::error::corrupt;

/// A whole-second instant, the precision the task wire form keeps, so a column written from a
/// value compares exactly with the same value read back from `body`.
pub(crate) fn whole_seconds(at: OffsetDateTime) -> Result<OffsetDateTime, TasksError> {
    OffsetDateTime::from_unix_timestamp(at.unix_timestamp())
        .map_err(|_| crate::error::failed("an instant is out of range"))
}

/// The task a row holds, after checking its columns against its body.
pub(crate) fn task_from_row(row: &Row) -> Result<Task, TasksError> {
    let body: Json = row.try_get("body").map_err(|_| corrupt("body"))?;
    let task: Task = serde_json::from_value(body).map_err(|_| corrupt("body shape"))?;
    let id: String = row.try_get("task_id").map_err(|_| corrupt("task_id"))?;
    let case: String = row.try_get("case_id").map_err(|_| corrupt("case_id"))?;
    let node: String = row.try_get("node").map_err(|_| corrupt("node"))?;
    let status: String = row.try_get("status").map_err(|_| corrupt("status"))?;
    let version: i64 = row.try_get("version").map_err(|_| corrupt("version"))?;
    let agrees = TaskId::new(&id).is_some_and(|id| id == task.id)
        && case == task.case
        && node == task.node
        && status == task.status.as_str()
        && u64::try_from(version).is_ok_and(|v| v == task.version);
    if agrees {
        Ok(task)
    } else {
        Err(corrupt("columns disagree with body"))
    }
}

/// The event a row holds, after checking its columns against its body.
pub(crate) fn event_from_row(row: &Row) -> Result<TaskEvent, TasksError> {
    let body: Json = row.try_get("body").map_err(|_| corrupt("event body"))?;
    let event: TaskEvent = serde_json::from_value(body).map_err(|_| corrupt("event body shape"))?;
    let id: String = row.try_get("task_id").map_err(|_| corrupt("event task"))?;
    let seq: i64 = row.try_get("seq").map_err(|_| corrupt("event seq"))?;
    let action: String = row.try_get("action").map_err(|_| corrupt("event action"))?;
    let agrees = id == event.task.as_str()
        && u64::try_from(seq).is_ok_and(|s| s == event.seq)
        && action == event.action.as_str();
    if agrees {
        Ok(event)
    } else {
        Err(corrupt("event columns disagree with body"))
    }
}
