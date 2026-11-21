//! Rows to Case records and back: the run header, the node map, the engine attempt, the wait and
//! the child link, with every node id validated on the way in.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{Attempt, AttemptState, ChildLink, EngineResult, TimerRecord};
use spot_dag::NodeId;
use spot_dag_exec::{Instant, NodeState, Run, RunId, RunStatus, TakenEdge};
use tokio_postgres::Row;

use crate::error::corrupt;

/// A stored instant for a parameter.
pub(crate) fn at(value: Instant) -> EngineResult<OffsetDateTime> {
    value
        .to_offset()
        .map_err(|error| corrupt(error.to_string()))
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
pub(crate) fn run_status(text: &str) -> EngineResult<RunStatus> {
    match text {
        "running" => Ok(RunStatus::Running),
        "completed" => Ok(RunStatus::Completed),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        other => Err(corrupt(format!("unknown run status `{other}`"))),
    }
}

/// The `state` text of an engine attempt.
pub(crate) fn attempt_state_text(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Started => "started",
        AttemptState::Waiting => "waiting",
        AttemptState::Retrying => "retrying",
        AttemptState::Done => "done",
        AttemptState::Failed => "failed",
    }
}

/// An engine attempt state from its `state` text.
pub(crate) fn attempt_state(text: &str) -> EngineResult<AttemptState> {
    match text {
        "started" => Ok(AttemptState::Started),
        "waiting" => Ok(AttemptState::Waiting),
        "retrying" => Ok(AttemptState::Retrying),
        "done" => Ok(AttemptState::Done),
        "failed" => Ok(AttemptState::Failed),
        other => Err(corrupt(format!("unknown attempt state `{other}`"))),
    }
}

/// The node map as the `jsonb` the `nodes` column holds.
pub(crate) fn nodes_to_json(nodes: &BTreeMap<NodeId, NodeState>) -> EngineResult<Json> {
    serde_json::to_value(nodes).map_err(|error| corrupt(format!("node states: {error}")))
}

/// The node map from the `nodes` column, validating every node id.
pub(crate) fn nodes_from_json(value: &Json) -> EngineResult<BTreeMap<NodeId, NodeState>> {
    let raw: BTreeMap<String, NodeState> = serde_json::from_value(value.clone())
        .map_err(|error| corrupt(format!("stored node states: {error}")))?;
    raw.into_iter()
        .map(|(node, state)| {
            let id =
                NodeId::new(&node).map_err(|error| corrupt(format!("stored node id: {error}")))?;
            Ok((id, state))
        })
        .collect()
}

/// The taken edges as the `jsonb` the `taken` column holds.
pub(crate) fn taken_to_json(taken: &BTreeSet<TakenEdge>) -> EngineResult<Json> {
    serde_json::to_value(taken).map_err(|error| corrupt(format!("taken edges: {error}")))
}

/// The taken edges from the `taken` column.
pub(crate) fn taken_from_json(value: &Json) -> EngineResult<BTreeSet<TakenEdge>> {
    serde_json::from_value(value.clone())
        .map_err(|error| corrupt(format!("stored taken edges: {error}")))
}

/// The child links as the `jsonb` the `children` column holds.
pub(crate) fn children_to_json(children: &BTreeMap<NodeId, ChildLink>) -> EngineResult<Json> {
    serde_json::to_value(children).map_err(|error| corrupt(format!("child links: {error}")))
}

/// The child links from the `children` column, validating every node id.
pub(crate) fn children_from_json(value: &Json) -> EngineResult<BTreeMap<NodeId, ChildLink>> {
    let raw: BTreeMap<String, ChildLink> = serde_json::from_value(value.clone())
        .map_err(|error| corrupt(format!("stored child links: {error}")))?;
    raw.into_iter()
        .map(|(node, link)| {
            let id =
                NodeId::new(&node).map_err(|error| corrupt(format!("stored node id: {error}")))?;
            Ok((id, link))
        })
        .collect()
}

/// A run from a `spot_bpm_case` row, with an empty node map (the caller fills it).
pub(crate) fn run_from_row(row: &Row) -> EngineResult<Run> {
    let id: String = row.try_get("case_id").map_err(|e| corrupt(e.to_string()))?;
    let id = RunId::new(&id).map_err(|error| corrupt(format!("stored case id: {error}")))?;
    let status: String = row.try_get("status").map_err(|e| corrupt(e.to_string()))?;
    let started_at: OffsetDateTime = row
        .try_get("started_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let ended_at: Option<OffsetDateTime> = row
        .try_get("ended_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let version: i64 = row.try_get("version").map_err(|e| corrupt(e.to_string()))?;
    let version = u64::try_from(version).map_err(|_| corrupt("negative case version"))?;
    let taken: Json = row.try_get("taken").map_err(|e| corrupt(e.to_string()))?;
    let nodes: Json = row.try_get("nodes").map_err(|e| corrupt(e.to_string()))?;
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
        key: row
            .try_get("business_key")
            .map_err(|e| corrupt(e.to_string()))?,
        status: run_status(&status)?,
        started_at: instant(started_at),
        ended_at: ended_at.map(instant),
        version,
        nodes: nodes_from_json(&nodes)?,
        taken: taken_from_json(&taken)?,
    })
}

/// The `initiator` column of a `spot_bpm_case` row.
pub(crate) fn initiator_from_row(row: &Row) -> EngineResult<Option<String>> {
    row.try_get("initiator").map_err(|e| corrupt(e.to_string()))
}

/// The engine attempt of a `spot_bpm_node_outcome` row.
pub(crate) fn attempt_from_row(row: &Row) -> EngineResult<Attempt> {
    let node: String = row.try_get("node").map_err(|e| corrupt(e.to_string()))?;
    let node = NodeId::new(&node).map_err(|error| corrupt(format!("stored node id: {error}")))?;
    let attempt: i32 = row.try_get("attempt").map_err(|e| corrupt(e.to_string()))?;
    let state: String = row.try_get("state").map_err(|e| corrupt(e.to_string()))?;
    let output: Option<Json> = row.try_get("output").map_err(|e| corrupt(e.to_string()))?;
    let branches: Json = row
        .try_get("branches")
        .map_err(|e| corrupt(e.to_string()))?;
    let failure: Option<String> = row.try_get("failure").map_err(|e| corrupt(e.to_string()))?;
    let started_at: OffsetDateTime = row
        .try_get("started_at")
        .map_err(|e| corrupt(e.to_string()))?;
    let not_before: Option<OffsetDateTime> = row
        .try_get("not_before")
        .map_err(|e| corrupt(e.to_string()))?;
    let is_retry: bool = row
        .try_get("is_retry")
        .map_err(|e| corrupt(e.to_string()))?;
    let branches: Vec<String> = serde_json::from_value(branches)
        .map_err(|error| corrupt(format!("stored branches: {error}")))?;
    Ok(Attempt {
        node,
        attempt: u32::try_from(attempt).unwrap_or(0),
        state: attempt_state(&state)?,
        output,
        branches,
        failure,
        started_at: instant(started_at),
        not_before: not_before.map(instant),
        is_retry,
    })
}

/// The wait of a `spot_bpm_node_outcome` row, when it carries one.
pub(crate) fn timer_from_row(row: &Row) -> EngineResult<Option<TimerRecord>> {
    let due: Option<OffsetDateTime> = row.try_get("due").map_err(|e| corrupt(e.to_string()))?;
    let event: Option<String> = row.try_get("event").map_err(|e| corrupt(e.to_string()))?;
    let human: Option<String> = row.try_get("human").map_err(|e| corrupt(e.to_string()))?;
    if due.is_none() && event.is_none() && human.is_none() {
        return Ok(None);
    }
    let node: String = row.try_get("node").map_err(|e| corrupt(e.to_string()))?;
    let node = NodeId::new(&node).map_err(|error| corrupt(format!("stored node id: {error}")))?;
    let attempt: i32 = row.try_get("attempt").map_err(|e| corrupt(e.to_string()))?;
    Ok(Some(TimerRecord {
        node,
        attempt: u32::try_from(attempt).unwrap_or(0),
        due: due.map(instant),
        event,
        human,
    }))
}
