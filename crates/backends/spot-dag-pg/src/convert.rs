//! Rows to runs and node states, and the instants and status texts they carry.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_dag::NodeId;
use spot_dag_exec::{ExecResult, Instant, NodeState, NodeStatus, Run, RunId, RunStatus, TakenEdge};
use tokio_postgres::Row;

use crate::error::corrupt;

/// A stored instant for a parameter.
pub(crate) fn at(value: Instant) -> ExecResult<OffsetDateTime> {
    value.to_offset()
}

/// A stored instant from a column.
pub(crate) fn instant(value: OffsetDateTime) -> Instant {
    Instant::from_offset(value)
}

/// The `status` text of a run.
pub(crate) fn run_status_text(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

/// A run status from its `status` text.
pub(crate) fn run_status(text: &str) -> ExecResult<RunStatus> {
    match text {
        "running" => Ok(RunStatus::Running),
        "completed" => Ok(RunStatus::Completed),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        other => Err(corrupt(format!("unknown run status `{other}`"))),
    }
}

/// The `status` text of a node.
pub(crate) fn node_status_text(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "pending",
        NodeStatus::Ready => "ready",
        NodeStatus::Running => "running",
        NodeStatus::Done => "done",
        NodeStatus::Skipped => "skipped",
        NodeStatus::Failed => "failed",
    }
}

/// A node status from its `status` text.
pub(crate) fn node_status(text: &str) -> ExecResult<NodeStatus> {
    match text {
        "pending" => Ok(NodeStatus::Pending),
        "ready" => Ok(NodeStatus::Ready),
        "running" => Ok(NodeStatus::Running),
        "done" => Ok(NodeStatus::Done),
        "skipped" => Ok(NodeStatus::Skipped),
        "failed" => Ok(NodeStatus::Failed),
        other => Err(corrupt(format!("unknown node status `{other}`"))),
    }
}

/// The taken edges as the `jsonb` the `taken` column holds.
pub(crate) fn taken_to_json(taken: &BTreeSet<TakenEdge>) -> ExecResult<Json> {
    serde_json::to_value(taken).map_err(|error| corrupt(format!("taken edges: {error}")))
}

/// The taken edges from the `taken` column.
pub(crate) fn taken_from_json(value: &Json) -> ExecResult<BTreeSet<TakenEdge>> {
    serde_json::from_value(value.clone())
        .map_err(|error| corrupt(format!("stored taken edges: {error}")))
}

/// A run from a `spot_dag_run` row, with an empty node map (the caller fills it).
pub(crate) fn run_from_row(row: &Row) -> ExecResult<Run> {
    let id: String = row.try_get("id").map_err(|e| corrupt(e.to_string()))?;
    let id = RunId::new(&id).map_err(|e| corrupt(format!("stored run id: {e}")))?;
    let status: String = row.try_get("status").map_err(|e| corrupt(e.to_string()))?;
    let started_at: OffsetDateTime = row
        .try_get("started_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let ended_at: Option<OffsetDateTime> = row
        .try_get("ended_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let version: i64 = row.try_get("version").map_err(|e| corrupt(e.to_string()))?;
    let version = u64::try_from(version).map_err(|_| corrupt("negative run version"))?;
    let taken: Json = row.try_get("taken").map_err(|e| corrupt(e.to_string()))?;
    let dag_version: i32 = row
        .try_get("dag_version")
        .map_err(|e| corrupt(e.to_string()))?;
    Ok(Run {
        id,
        dag_key: row.try_get("dag_key").map_err(|e| corrupt(e.to_string()))?,
        dag_version: u32::try_from(dag_version).unwrap_or(0),
        content_hash: row
            .try_get("content_hash")
            .map_err(|e| corrupt(e.to_string()))?,
        key: row.try_get("key").map_err(|e| corrupt(e.to_string()))?,
        status: run_status(&status)?,
        started_at: instant(started_at),
        ended_at: ended_at.map(instant),
        version,
        nodes: BTreeMap::new(),
        taken: taken_from_json(&taken)?,
    })
}

/// One node state from a `spot_dag_node_state` row.
pub(crate) fn node_from_row(row: &Row) -> ExecResult<(NodeId, NodeState)> {
    let node: String = row.try_get("node").map_err(|e| corrupt(e.to_string()))?;
    let node = NodeId::new(&node).map_err(|e| corrupt(format!("stored node id: {e}")))?;
    let status: String = row.try_get("status").map_err(|e| corrupt(e.to_string()))?;
    let attempts: i32 = row
        .try_get("attempts")
        .map_err(|e| corrupt(e.to_string()))?;
    let output: Option<Json> = row.try_get("output").map_err(|e| corrupt(e.to_string()))?;
    let failure: Option<String> = row.try_get("failure").map_err(|e| corrupt(e.to_string()))?;
    let started_at: Option<OffsetDateTime> = row
        .try_get("started_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let ended_at: Option<OffsetDateTime> = row
        .try_get("ended_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let state = NodeState {
        status: node_status(&status)?,
        attempts: u32::try_from(attempts).unwrap_or(0),
        output,
        failure,
        started_at: started_at.map(instant),
        ended_at: ended_at.map(instant),
    };
    Ok((node, state))
}
