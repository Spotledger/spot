//! Process insight, part b (WP-BPM-11.T4): the three insight reports registered through the
//! report-source port and the fold scheduled through the internal-schedule port.
//!
//! # What it does
//!
//! `spot-bpm-insight` ships three report definitions; their data is no DocType but the insight the
//! process-insight module folds from the node-run log. Each definition is offered to the report
//! engine as a **report source** ([`ReportSources`], WP-BPM-11.T3): an in-memory schema for the base
//! name, an executor that answers the engine's one storage query from [`ProcessInsightPort`], and a
//! visibility hook that asks the port who may see what. One internal schedule,
//! `process_insight:fold`, calls [`ProcessInsightPort::fold_once`] ([`InternalSchedules`]).
//!
//! The executor never runs SQL: it evaluates the compiled query IR over rows built from a snapshot
//! (exact integers; filters, groups, `count`/`sum`/`min`/`max`, sorts, limit, offset). A request
//! value is only ever a compared literal. A construct it does not know answers `storage.unsupported`.
//!
//! # What the snapshot can say
//!
//! Insight keeps one aggregate per step, not one row per attempt, so the cycle-time source serves
//! one row per step: `count` of a measure is the step's completed attempts, `sum` of `cycle_ms`,
//! `work_ms`, `wait_ms` its totals, `min`/`max` of `cycle_ms` its shortest and longest cycle. No
//! longest wait is kept (null). The stuck queue shows ids, node, reason code (in `state`) and owner;
//! `node_kind` and `started_at` are null. The throughput report needs a per-week completion feed
//! ([`ThroughputFeed`]) no module provides yet: without it the report is not registered.
//!
//! # Who sees what
//!
//! The hook asks [`ProcessInsightPort::access`] which processes the caller may read and admits only
//! those: a process they may not read is absent, never a row of zeros. The stuck queue asks
//! [`ProcessInsightPort::stuck`] (initiator, assignee or `process_supervise` holder). A failing port
//! denies (`404 report.not_found`, audited with its reason code). The report's `roles` decide only
//! who sees that it exists.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use spot_bpm_engine::NodeKind;
use spot_bpm_insight::{
    CYCLE_TIME_BY_STEP, Insight, NODE_RUN_DOCTYPE, ProcessKey, RUN_DOCTYPE, RunLabels, STUCK_QUEUE,
    StepStats, THROUGHPUT_BY_WEEK, report_definitions,
};
use spot_core::SiteId;
use spot_jobs::worker::{BoxFuture as JobFuture, HandlerResult, JobContext};
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_reports::BoxFuture;
use spot_scheduler::MissedRunPolicy;
use spot_storage_contract::{
    AggFunc, CmpOp, ErrorKind, Expr, Name, Nulls, OrderDir, Query, Rows, StorageError, Value,
};
use time::OffsetDateTime;

use crate::services::SiteCtx;
use crate::services::jobs::{InternalHandler, InternalSchedule, InternalSchedules};
use crate::services::process_insight::{self, InsightPortError, ProcessInsightPort, StuckEntry};
use crate::services::report::{
    Admission, AdmitRequest, Denied, ReportSource, ReportSources, SourceExecutor, SourceVisibility,
};
use crate::{Error, Result, error};

/// Start order: after the process-insight module (1190), whose port this one fetches.
pub const ORDER: u32 = 1195;

/// The service name that namespaces the schedule (`process_insight:fold`) and is the principal the
/// fold runs as in the audit trail.
pub const SERVICE: &str = "process_insight";

/// The schedule's name.
pub const FOLD_SCHEDULE: &str = "fold";

/// The roles that see the reports exist when `roles` is not set.
pub const DEFAULT_ROLES: [&str; 3] = [
    "Tenant Administrator",
    "Process Supervisor",
    "Process Reader",
];

// Settings

/// `[services.process_insight_reports]`. Unknown keys are refused. (The fold's interval is
/// `fold_interval_secs` of `[services.process_insight]`.)
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the reports and the schedule are registered at all.
    pub enabled: bool,
    /// The roles that see the three reports exist (the data stays filtered by the visibility
    /// hook).
    pub roles: Vec<String>,
    /// The singular label of a Run in titles and headings (ADR-080). Default `Request`.
    pub run_label_singular: Option<String>,
    /// The plural label of a Run in titles and headings (ADR-080). Default `Requests`.
    pub run_label_plural: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            roles: DEFAULT_ROLES.iter().map(|r| (*r).to_owned()).collect(),
            run_label_singular: None,
            run_label_plural: None,
        }
    }
}

// The throughput seam

/// Completed Runs of one process version in one ISO week.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeeklyThroughput {
    /// The process name.
    pub process: String,
    /// The process version.
    pub version: u32,
    /// The ISO week of the Runs' end, as `YYYY-Www`.
    pub week: String,
    /// How many Runs ended in that week.
    pub completed: u64,
}

/// Where completed Runs per week come from. Optional: the insight snapshot keeps per-step
/// aggregates only, so nothing in the host provides this yet and `bpm_throughput_by_week` is not
/// registered without it. A module that folds Run completions provides it with
/// `ctx.provide::<dyn ThroughputFeed>(..)` before this module (`ORDER` below 1195).
pub trait ThroughputFeed: Send + Sync {
    /// The weekly completions of `site`; `Err(())` means the feed cannot answer.
    #[allow(clippy::result_unit_err)]
    fn weeks(&self, site: &SiteId) -> std::result::Result<Vec<WeeklyThroughput>, ()>;
}

// Source schemas

fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

/// The in-memory schema of `bpm_node_run`: one row per node attempt (served as one per step).
#[must_use]
pub fn node_run_schema() -> Definition {
    let mut def = Definition::new(NODE_RUN_DOCTYPE);
    def.fields = vec![
        field("process", FieldType::Text),
        field("process_version", FieldType::Int),
        field("node", FieldType::Text),
        field("node_kind", FieldType::Text),
        field("run", FieldType::Text),
        field("state", FieldType::Text),
        field("owner", FieldType::Text),
        field("started_at", FieldType::DateTime),
        field("ended_at", FieldType::DateTime),
        field("cycle_ms", FieldType::Int),
        field("work_ms", FieldType::Int),
        field("wait_ms", FieldType::Int),
        field("failure_code", FieldType::Text),
        field("attempt", FieldType::Int),
    ];
    def
}

/// The in-memory schema of `bpm_case`: one row per Run (served as one per process version and
/// week).
#[must_use]
pub fn run_schema() -> Definition {
    let mut def = Definition::new(RUN_DOCTYPE);
    def.fields = vec![
        field("process", FieldType::Text),
        field("process_version", FieldType::Int),
        field("status", FieldType::Text),
        field("started_at", FieldType::DateTime),
        field("ended_at", FieldType::DateTime),
        field("week", FieldType::Text),
    ];
    def
}

// A small evaluator of the storage query IR over snapshot rows

/// The measures of one step, in whole milliseconds and counts.
#[derive(Clone, Copy, Debug, Default)]
struct Facts {
    attempts: i64,
    completed: i64,
    total: i64,
    work: i64,
    wait: i64,
    min: Option<i64>,
    max: Option<i64>,
}

/// One row of a snapshot: the dimension values by field name and, for a cube row, its measures.
#[derive(Clone, Debug)]
struct SnapRow {
    dims: BTreeMap<&'static str, Value>,
    facts: Option<Facts>,
}

fn unsupported(what: &str) -> StorageError {
    StorageError::new(
        ErrorKind::Unsupported,
        format!("the insight source does not evaluate {what}"),
    )
}

fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn text(v: &str) -> Value {
    Value::Text(v.to_owned())
}

/// The measure `func` over `field` of one cube row.
fn measure(facts: &Facts, func: AggFunc, field: &str) -> std::result::Result<Value, StorageError> {
    let known = matches!(
        field,
        "attempt" | "cycle_ms" | "work_ms" | "wait_ms" | "ended_at"
    );
    if !known {
        return Err(unsupported("an aggregate over that field"));
    }
    Ok(match func {
        AggFunc::Count => Value::Int(facts.completed),
        AggFunc::Sum => match field {
            "cycle_ms" => Value::Int(facts.total),
            "work_ms" => Value::Int(facts.work),
            "wait_ms" => Value::Int(facts.wait),
            "attempt" => Value::Int(facts.attempts),
            _ => return Err(unsupported("a sum over that field")),
        },
        AggFunc::Max => match field {
            "cycle_ms" => facts.max.map_or(Value::Null, Value::Int),
            // No longest work or wait is kept per step.
            _ => Value::Null,
        },
        AggFunc::Min => match field {
            "cycle_ms" => facts.min.map_or(Value::Null, Value::Int),
            _ => Value::Null,
        },
    })
}

/// Combines the per-row measures of one group.
fn combine(func: AggFunc, values: &[Value]) -> Value {
    let ints: Vec<i64> = values
        .iter()
        .filter_map(|v| match v {
            Value::Int(i) => Some(*i),
            _ => None,
        })
        .collect();
    match func {
        AggFunc::Count => Value::Int(ints.iter().fold(0_i64, |a, b| a.saturating_add(*b))),
        AggFunc::Sum if ints.is_empty() => Value::Null,
        AggFunc::Sum => Value::Int(ints.iter().fold(0_i64, |a, b| a.saturating_add(*b))),
        AggFunc::Max => ints.iter().max().map_or(Value::Null, |m| Value::Int(*m)),
        AggFunc::Min => ints.iter().min().map_or(Value::Null, |m| Value::Int(*m)),
    }
}

/// A scalar expression over one row.
fn scalar(expr: &Expr, row: &SnapRow) -> std::result::Result<Value, StorageError> {
    match expr {
        Expr::Field(f) => row
            .dims
            .get(f.field.as_str())
            .cloned()
            .ok_or_else(|| unsupported("a field outside an aggregate")),
        Expr::Value(v) => Ok(v.clone()),
        Expr::Coalesce(list) => {
            for e in list {
                let v = scalar(e, row)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        _ => Err(unsupported("that expression")),
    }
}

/// Orders two non-null values of one kind.
fn compare(a: &Value, b: &Value) -> std::result::Result<Ordering, StorageError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Text(x), Value::Text(y)) => Ok(x.cmp(y)),
        (Value::DateTime(x), Value::DateTime(y)) => Ok(x.cmp(y)),
        _ => Err(StorageError::new(
            ErrorKind::InvalidQuery,
            "a comparison between values of different kinds",
        )),
    }
}

/// Three-valued truth of a filter over one row (`None` is unknown).
fn truth(expr: &Expr, row: &SnapRow) -> std::result::Result<Option<bool>, StorageError> {
    match expr {
        Expr::Cmp(a, op, b) => {
            let (x, y) = (scalar(a, row)?, scalar(b, row)?);
            if x.is_null() || y.is_null() {
                return Ok(None);
            }
            let ord = compare(&x, &y)?;
            Ok(Some(match op {
                CmpOp::Eq => ord == Ordering::Equal,
                CmpOp::Ne => ord != Ordering::Equal,
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
            }))
        }
        Expr::And(list) => {
            let mut unknown = false;
            for e in list {
                match truth(e, row)? {
                    Some(false) => return Ok(Some(false)),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            Ok(if unknown { None } else { Some(true) })
        }
        Expr::Or(list) => {
            let mut unknown = false;
            for e in list {
                match truth(e, row)? {
                    Some(true) => return Ok(Some(true)),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            Ok(if unknown { None } else { Some(false) })
        }
        Expr::Not(e) => Ok(truth(e, row)?.map(|t| !t)),
        Expr::IsNull(e) => Ok(Some(scalar(e, row)?.is_null())),
        Expr::In(e, values) => {
            let x = scalar(e, row)?;
            if x.is_null() {
                return Ok(None);
            }
            for v in values {
                if !v.is_null() && compare(&x, v)? == Ordering::Equal {
                    return Ok(Some(true));
                }
            }
            Ok(Some(false))
        }
        Expr::Value(Value::Bool(b)) => Ok(Some(*b)),
        _ => Err(unsupported("that filter")),
    }
}

fn has_agg(expr: &Expr) -> bool {
    match expr {
        Expr::Agg { .. } => true,
        Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => has_agg(a) || has_agg(b),
        Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => v.iter().any(has_agg),
        Expr::Not(e) | Expr::IsNull(e) | Expr::In(e, _) => has_agg(e),
        _ => false,
    }
}

/// An expression over one group of rows: an aggregate over the group's measures, or a dimension of
/// its first row (a group key).
fn group_value(expr: &Expr, rows: &[&SnapRow]) -> std::result::Result<Value, StorageError> {
    match expr {
        Expr::Agg {
            func,
            arg,
            distinct,
        } => {
            if *distinct {
                return Err(unsupported("a distinct aggregate"));
            }
            let field = match arg.as_deref() {
                Some(Expr::Field(f)) => f.field.as_str(),
                // A count with no argument counts the group's completed attempts.
                None if *func == AggFunc::Count => "attempt",
                _ => return Err(unsupported("an aggregate over an expression")),
            };
            let mut values = Vec::with_capacity(rows.len());
            for row in rows {
                let facts = row
                    .facts
                    .as_ref()
                    .ok_or_else(|| unsupported("an aggregate over this source"))?;
                values.push(measure(facts, *func, field)?);
            }
            Ok(combine(*func, &values))
        }
        Expr::Value(v) => Ok(v.clone()),
        other => match rows.first() {
            Some(first) => scalar(other, first),
            None => Ok(Value::Null),
        },
    }
}

fn order_values(
    a: &Value,
    b: &Value,
    dir: OrderDir,
    nulls: Nulls,
) -> std::result::Result<Ordering, StorageError> {
    Ok(match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => match nulls {
            Nulls::First => Ordering::Less,
            Nulls::Last => Ordering::Greater,
        },
        (false, true) => match nulls {
            Nulls::First => Ordering::Greater,
            Nulls::Last => Ordering::Less,
        },
        (false, false) => {
            let ord = compare(a, b)?;
            match dir {
                OrderDir::Asc => ord,
                OrderDir::Desc => ord.reverse(),
            }
        }
    })
}

/// Evaluates `query` over `rows`: filter, group, aggregate, sort, offset and limit.
fn evaluate(query: &Query, rows: &[SnapRow]) -> std::result::Result<Rows, StorageError> {
    if !query.joins.is_empty() {
        return Err(unsupported("a join"));
    }
    if query.having.is_some() {
        return Err(unsupported("a having clause"));
    }
    let mut kept: Vec<&SnapRow> = Vec::new();
    for row in rows {
        let pass = match &query.filter {
            Some(filter) => truth(filter, row)? == Some(true),
            None => true,
        };
        if pass {
            kept.push(row);
        }
    }

    let aggregating = !query.group_by.is_empty() || query.select.iter().any(|s| has_agg(&s.expr));
    let mut groups: Vec<Vec<&SnapRow>> = Vec::new();
    if aggregating {
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in &kept {
            let mut key = String::new();
            for expr in &query.group_by {
                key.push_str(&format!("{:?}\u{1f}", scalar(expr, row)?));
            }
            let at = *index.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[at].push(*row);
        }
        if query.group_by.is_empty() && groups.is_empty() {
            // An aggregate with no group over no rows is one row (count 0, the rest null).
            groups.push(Vec::new());
        }
    } else {
        groups.extend(kept.iter().map(|row| vec![*row]));
    }

    // Sort: the order keys, stable, so ties keep the snapshot's order.
    let mut keyed: Vec<(Vec<Value>, Vec<&SnapRow>)> = Vec::with_capacity(groups.len());
    for group in groups {
        let mut keys = Vec::with_capacity(query.order_by.len());
        for order in &query.order_by {
            keys.push(group_value(&order.expr, &group)?);
        }
        keyed.push((keys, group));
    }
    let mut failure: Option<StorageError> = None;
    keyed.sort_by(|a, b| {
        for (i, order) in query.order_by.iter().enumerate() {
            match order_values(&a.0[i], &b.0[i], order.dir, order.nulls) {
                Ok(Ordering::Equal) => {}
                Ok(other) => return other,
                Err(e) => {
                    failure.get_or_insert(e);
                    return Ordering::Equal;
                }
            }
        }
        Ordering::Equal
    });
    if let Some(e) = failure {
        return Err(e);
    }

    let columns: Vec<Name> = query.select.iter().map(|s| s.alias.clone()).collect();
    let skip = usize::try_from(query.offset).unwrap_or(usize::MAX);
    let take = query
        .limit
        .map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut out = Vec::new();
    for (_, group) in keyed.into_iter().skip(skip).take(take) {
        let mut row = Vec::with_capacity(query.select.len());
        for selection in &query.select {
            row.push(group_value(&selection.expr, &group)?);
        }
        out.push(row);
    }
    Ok(Rows { columns, rows: out })
}

// Rows of the snapshots

/// The kind of a node as the report states it.
fn kind_text(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Trigger => "trigger",
        NodeKind::Document => "document",
        NodeKind::Decision => "decision",
        NodeKind::Integration => "integration",
        NodeKind::Logic(_) => "logic",
        NodeKind::Wait => "wait",
        NodeKind::SubProcess => "sub_process",
        NodeKind::Agent => "agent",
        _ => "other",
    }
}

fn admits(admission: &Admission, process: &str) -> bool {
    match admission {
        Admission::All => true,
        Admission::Rows(names) => names.contains(process),
    }
}

fn step_row(step: &StepStats) -> SnapRow {
    let mut dims: BTreeMap<&'static str, Value> = BTreeMap::new();
    dims.insert("process", text(&step.process.name));
    dims.insert(
        "process_version",
        Value::Int(i64::from(step.process.version)),
    );
    dims.insert("node", text(step.node.as_str()));
    dims.insert("node_kind", text(kind_text(&step.kind)));
    for name in [
        "run",
        "state",
        "owner",
        "started_at",
        "ended_at",
        "cycle_ms",
        "work_ms",
        "wait_ms",
        "failure_code",
        "attempt",
    ] {
        dims.insert(name, Value::Null);
    }
    SnapRow {
        dims,
        facts: Some(Facts {
            attempts: int(step.attempts),
            completed: int(step.completed),
            total: step.total.as_millis(),
            work: step.work.as_millis(),
            wait: step.wait.as_millis(),
            min: step.min.map(spot_bpm_insight::Millis::as_millis),
            max: step.max.map(spot_bpm_insight::Millis::as_millis),
        }),
    }
}

/// The steps of `insight` the admission lets through, as rows.
fn step_rows(insight: &Insight, admission: &Admission) -> Vec<SnapRow> {
    insight
        .steps()
        .iter()
        .filter(|s| admits(admission, &s.process.name))
        .map(step_row)
        .collect()
}

/// A stuck Run as an admission key: the hook asks the port, the executor reads the answer back.
/// Keys are opaque to the report engine.
fn stuck_key(entry: &StuckEntry) -> String {
    json!({
        "p": entry.process,
        "v": entry.version,
        "r": entry.run,
        "n": entry.node,
        "c": entry.reason,
        "o": entry.owner,
    })
    .to_string()
}

fn stuck_row(key: &str) -> Option<SnapRow> {
    let v: serde_json::Value = serde_json::from_str(key).ok()?;
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let mut dims: BTreeMap<&'static str, Value> = BTreeMap::new();
    dims.insert("process", Value::Text(s("p")?));
    dims.insert("process_version", Value::Int(v.get("v")?.as_i64()?));
    dims.insert("run", Value::Text(s("r")?));
    dims.insert("node", Value::Text(s("n")?));
    dims.insert("state", Value::Text(s("c")?));
    dims.insert("owner", s("o").map_or(Value::Null, Value::Text));
    for name in [
        "node_kind",
        "started_at",
        "ended_at",
        "cycle_ms",
        "work_ms",
        "wait_ms",
        "failure_code",
        "attempt",
    ] {
        dims.insert(name, Value::Null);
    }
    Some(SnapRow { dims, facts: None })
}

fn week_row(week: &WeeklyThroughput) -> SnapRow {
    let mut dims: BTreeMap<&'static str, Value> = BTreeMap::new();
    dims.insert("process", text(&week.process));
    dims.insert("process_version", Value::Int(i64::from(week.version)));
    dims.insert("week", text(&week.week));
    for name in ["status", "started_at", "ended_at"] {
        dims.insert(name, Value::Null);
    }
    SnapRow {
        dims,
        facts: Some(Facts {
            completed: int(week.completed),
            ..Facts::default()
        }),
    }
}

fn store_error(code: &str) -> StorageError {
    StorageError::new(
        ErrorKind::Unavailable,
        format!("the insight source is unavailable ({code})"),
    )
}

// Executors

/// Answers `bpm_cycle_time_by_step` from the insight snapshot.
struct CycleTimeExecutor {
    port: Arc<dyn ProcessInsightPort>,
    site: SiteId,
}

impl SourceExecutor for CycleTimeExecutor {
    fn execute<'a>(
        &'a self,
        admission: &'a Admission,
        query: &'a Query,
    ) -> BoxFuture<'a, std::result::Result<Rows, StorageError>> {
        Box::pin(async move {
            let insight = self
                .port
                .snapshot(&self.site)
                .map_err(|e| store_error(e.code()))?;
            evaluate(query, &step_rows(&insight, admission))
        })
    }
}

/// Answers `bpm_stuck_queue` from the keys the hook admitted.
struct StuckExecutor;

impl SourceExecutor for StuckExecutor {
    fn execute<'a>(
        &'a self,
        admission: &'a Admission,
        query: &'a Query,
    ) -> BoxFuture<'a, std::result::Result<Rows, StorageError>> {
        Box::pin(async move {
            let rows: Vec<SnapRow> = match admission {
                Admission::Rows(keys) => keys.iter().filter_map(|k| stuck_row(k)).collect(),
                // The hook never admits "all" of the stuck queue: nothing is listed.
                Admission::All => Vec::new(),
            };
            evaluate(query, &rows)
        })
    }
}

/// Answers `bpm_throughput_by_week` from the throughput feed.
struct ThroughputExecutor {
    feed: Arc<dyn ThroughputFeed>,
    site: SiteId,
}

impl SourceExecutor for ThroughputExecutor {
    fn execute<'a>(
        &'a self,
        admission: &'a Admission,
        query: &'a Query,
    ) -> BoxFuture<'a, std::result::Result<Rows, StorageError>> {
        Box::pin(async move {
            let weeks = self
                .feed
                .weeks(&self.site)
                .map_err(|()| store_error("process_insight.throughput_unavailable"))?;
            let rows: Vec<SnapRow> = weeks
                .iter()
                .filter(|w| admits(admission, &w.process))
                .map(week_row)
                .collect();
            evaluate(query, &rows)
        })
    }
}

// Visibility hooks

fn site_of(request: &AdmitRequest) -> std::result::Result<SiteId, Denied> {
    SiteId::new(&request.site).map_err(|_| Denied("process_insight.site"))
}

/// The processes of `names` the caller may read, by the port's access decision.
fn readable(
    port: &dyn ProcessInsightPort,
    request: &AdmitRequest,
    names: impl IntoIterator<Item = ProcessKey>,
) -> BTreeSet<String> {
    let access = port.access(&request.principal);
    names
        .into_iter()
        .filter(|key| access.may_read(key))
        .map(|key| key.name)
        .collect()
}

/// Admits the processes the caller may read from the insight snapshot.
struct SnapshotVisibility {
    port: Arc<dyn ProcessInsightPort>,
}

impl SourceVisibility for SnapshotVisibility {
    fn admit<'a>(
        &'a self,
        request: &'a AdmitRequest,
    ) -> BoxFuture<'a, std::result::Result<Admission, Denied>> {
        Box::pin(async move {
            let site = site_of(request)?;
            let insight = self.port.snapshot(&site).map_err(|e| Denied(e.code()))?;
            let keys = insight.steps().iter().map(|s| s.process.clone());
            Ok(Admission::Rows(readable(&*self.port, request, keys)))
        })
    }
}

/// Admits the processes the caller may read from the throughput feed.
struct FeedVisibility {
    port: Arc<dyn ProcessInsightPort>,
    feed: Arc<dyn ThroughputFeed>,
}

impl SourceVisibility for FeedVisibility {
    fn admit<'a>(
        &'a self,
        request: &'a AdmitRequest,
    ) -> BoxFuture<'a, std::result::Result<Admission, Denied>> {
        Box::pin(async move {
            let site = site_of(request)?;
            let weeks = self
                .feed
                .weeks(&site)
                .map_err(|()| Denied("process_insight.throughput_unavailable"))?;
            let keys = weeks
                .iter()
                .map(|w| ProcessKey::new(w.process.clone(), w.version));
            Ok(Admission::Rows(readable(&*self.port, request, keys)))
        })
    }
}

/// Admits exactly the stuck Runs the port lists for the caller (initiator, assignee, supervisor).
struct StuckVisibility {
    port: Arc<dyn ProcessInsightPort>,
    clock: Arc<dyn spot_core::Clock>,
}

impl SourceVisibility for StuckVisibility {
    fn admit<'a>(
        &'a self,
        request: &'a AdmitRequest,
    ) -> BoxFuture<'a, std::result::Result<Admission, Denied>> {
        Box::pin(async move {
            let entries = self
                .port
                .stuck(&request.principal, self.clock.now_utc())
                .await
                .map_err(|e| Denied(e.code()))?;
            Ok(Admission::Rows(entries.iter().map(stuck_key).collect()))
        })
    }
}

// Registration

/// The report sources of process insight: the three definitions (the throughput one only when
/// `feed` is given), with their titles from the run labels and `roles` as the roles that see them.
///
/// # Errors
///
/// A shipped definition that does not parse or validate.
pub fn sources(
    port: &Arc<dyn ProcessInsightPort>,
    feed: Option<&Arc<dyn ThroughputFeed>>,
    site: &SiteId,
    clock: &Arc<dyn spot_core::Clock>,
    labels: &RunLabels,
    roles: &[String],
) -> Result<Vec<ReportSource>> {
    let mut out = Vec::new();
    for mut def in report_definitions(labels).map_err(error)? {
        def.roles = roles.to_vec();
        let (schema, executor, visibility): (
            Definition,
            Arc<dyn SourceExecutor>,
            Arc<dyn SourceVisibility>,
        ) = match def.name.as_str() {
            CYCLE_TIME_BY_STEP => (
                node_run_schema(),
                Arc::new(CycleTimeExecutor {
                    port: Arc::clone(port),
                    site: site.clone(),
                }),
                Arc::new(SnapshotVisibility {
                    port: Arc::clone(port),
                }),
            ),
            STUCK_QUEUE => (
                node_run_schema(),
                Arc::new(StuckExecutor),
                Arc::new(StuckVisibility {
                    port: Arc::clone(port),
                    clock: Arc::clone(clock),
                }),
            ),
            THROUGHPUT_BY_WEEK => match feed {
                Some(feed) => (
                    run_schema(),
                    Arc::new(ThroughputExecutor {
                        feed: Arc::clone(feed),
                        site: site.clone(),
                    }),
                    Arc::new(FeedVisibility {
                        port: Arc::clone(port),
                        feed: Arc::clone(feed),
                    }),
                ),
                None => continue,
            },
            _ => continue,
        };
        out.push(ReportSource {
            service: SERVICE.to_owned(),
            def: Arc::new(def),
            schema: vec![schema],
            executor,
            visibility,
            required: false,
        });
    }
    Ok(out)
}

/// The six-field cron expression of an interval of `secs` seconds: every `secs` seconds up to a
/// minute, otherwise every whole number of minutes (rounded down, at least one).
#[must_use]
pub fn fold_cron(secs: u64) -> String {
    if secs < 60 {
        format!("*/{} * * * * *", secs.max(1))
    } else {
        format!("0 */{} * * * *", (secs / 60).clamp(1, 59))
    }
}

/// The handler of `process_insight:fold`: one [`ProcessInsightPort::fold_once`]. A lost lease is
/// not a failure (another node folds); any other error asks for a retry by its code.
#[must_use]
pub fn fold_handler(port: Arc<dyn ProcessInsightPort>) -> InternalHandler {
    Arc::new(
        move |ctx: JobContext| -> JobFuture<'static, HandlerResult> {
            let port = Arc::clone(&port);
            Box::pin(async move {
                let now: OffsetDateTime = ctx.clock;
                match port.fold_once(now).await {
                    Ok(report) => {
                        tracing::debug!(
                            folded = report.folded,
                            standby = report.standby,
                            "process insight folded"
                        );
                        HandlerResult::Ok
                    }
                    Err(InsightPortError::LeaseLost) => HandlerResult::Ok,
                    Err(e) => {
                        tracing::warn!(code = e.code(), "the scheduled insight fold failed");
                        HandlerResult::Retry(e.code().to_owned())
                    }
                }
            })
        },
    )
}

/// Registers the insight reports with the report-source port and the fold with the
/// internal-schedule port. Inert when `services.process_insight_reports.enabled` or
/// `services.process_insight.enabled` is false, on a reload build, and when the insight port or a
/// registration port is absent (the module that provides it is off).
///
/// # Errors
///
/// A setting that does not decode; a registration a port refuses.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_insight_reports")?;
    let insight_settings: process_insight::Settings = ctx.settings_or_default("process_insight")?;
    if !settings.enabled || !insight_settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let Some(port) = ctx.port::<dyn ProcessInsightPort>() else {
        return Ok(());
    };
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(error)?;
    let labels = RunLabels::new(
        settings.run_label_singular.as_deref(),
        settings.run_label_plural.as_deref(),
    )
    .map_err(|e| Error(format!("services.process_insight_reports: {e}")))?;

    match ctx.port::<dyn ReportSources>() {
        Some(reports) => {
            let feed = ctx.port::<dyn ThroughputFeed>();
            let clock = Arc::clone(ctx.clock());
            for source in sources(
                &port,
                feed.as_ref(),
                &site,
                &clock,
                &labels,
                &settings.roles,
            )? {
                reports.register(source)?;
            }
            if feed.is_none() {
                tracing::info!(
                    report = THROUGHPUT_BY_WEEK,
                    "no throughput feed is provided: the throughput report is not registered"
                );
            }
        }
        None => tracing::info!("the report service is off: the insight reports are not registered"),
    }

    match ctx.port::<dyn InternalSchedules>() {
        Some(schedules) => schedules.register(InternalSchedule {
            service: SERVICE.to_owned(),
            name: FOLD_SCHEDULE.to_owned(),
            cron: fold_cron(insight_settings.fold_interval_secs),
            // Missed runs coalesce into one fold: the fold reads the log from its checkpoint, so
            // one run catches up everything.
            missed_run_policy: MissedRunPolicy::RunOnce,
            queue: "short".to_owned(),
            budget: Duration::from_secs(30),
            handler: fold_handler(port),
        })?,
        None => tracing::info!(
            "the jobs module has no schedule port: the insight fold is not scheduled"
        ),
    }
    Ok(())
}
