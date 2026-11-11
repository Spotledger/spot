//! Optional Case migration from one process version to another.
//!
//! [`plan_migration`] is pure: it takes a Case, the version it pins, the target version and an
//! explicit [`MigrationMapping`], and returns a [`MigrationPlan`]. The plan carries the exact
//! [`CaseRecord`] that would be stored, the node changes, the variable changes and a SHA-256
//! digest over its canonical form. [`MigrationPlan::dry_run`] renders the same information for
//! a person without writing anything.
//!
//! Applying goes through one port, [`MigrationTarget`], so the host writes the migrated record
//! and its [`MigrationAudit`] event in **one transaction**. [`MemoryMigrationTarget`] is the
//! reference implementation the tests use.
//!
//! # History is not rewritten
//!
//! A node that reached `Done`, `Skipped` or `Failed` keeps its id, its status, its output, its
//! times and the taken edges around it. Only nodes the Case can still run (`Pending`, `Ready`,
//! `Running`) are mapped onto the target version. The plan proves this with
//! [`MigrationPlan::history_preserved`].
//!
//! # Refusals
//!
//! A current node with no explicit pair and no same id in the target is refused with
//! `bpm_versioning.unmapped_node`; a pair naming a node the target does not define is refused
//! with `bpm_versioning.unknown_target_node`; two source nodes onto one target node is
//! `bpm_versioning.mapping_collision`; a required variable with no value and no default is
//! `bpm_versioning.variable_missing`.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, CaseRecord, ProcessDefinition};
use spot_dag::NodeId;
use spot_dag_exec::{NodeStatus, RunStatus, TakenEdge};

use crate::error::{Result, VersioningError};
use crate::mapping::MigrationMapping;
use crate::version::VersionRef;

/// One node the migration re-points.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeChange {
    /// The node the Case stores now.
    pub from: String,
    /// The node the target version calls it.
    pub to: String,
}

/// What happened to one required variable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableAction {
    /// The Case already carries the value; migration keeps it.
    Kept,
    /// The Case did not carry it; the mapping's default was written.
    Defaulted {
        /// The default that was written.
        value: Json,
    },
}

impl VariableAction {
    /// A short token for the dry-run text.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::Defaulted { .. } => "defaulted",
        }
    }
}

/// One dotted variable path in the target's contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VariableChange {
    /// The dotted path.
    pub name: String,
    /// Whether it was kept or defaulted.
    pub action: VariableAction,
}

/// The human-readable dry run of a migration: exactly what applying would change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DryRunReport {
    /// The Case.
    pub case: CaseId,
    /// The version the Case pins.
    pub from: VersionRef,
    /// The version it would move to.
    pub to: VersionRef,
    /// The current node under its old id.
    pub current_node: String,
    /// The current node under the target id.
    pub target_node: String,
    /// Every node the migration re-points.
    pub node_changes: Vec<NodeChange>,
    /// Every required variable and what happens to it.
    pub variable_changes: Vec<VariableChange>,
    /// Whether completed-node history stays exactly as stored.
    pub history_preserved: bool,
    /// The plan's digest.
    pub plan_digest: String,
}

/// The audit event a migration writes in the same transaction as the Case.
#[derive(Clone, Debug, PartialEq)]
pub struct MigrationAudit {
    /// The audit topic.
    pub topic: String,
    /// The Case migrated.
    pub case: CaseId,
    /// Where it came from.
    pub from: VersionRef,
    /// Where it went.
    pub to: VersionRef,
    /// The plan's digest.
    pub plan_digest: String,
    /// The human or service that asked for the migration.
    pub actor: Option<String>,
    /// The Case's store version when the plan was made.
    pub store_version: u64,
    /// How many nodes were re-pointed.
    pub nodes_changed: usize,
    /// How many variables were defaulted.
    pub variables_defaulted: usize,
    /// When the migration was applied.
    pub at: OffsetDateTime,
}

/// The stable audit topic of a Case migration.
pub const MIGRATION_AUDIT_TOPIC: &str = "sys_bpm_case_migrated";

/// A planned Case migration: the report, the digest and the exact record to store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MigrationPlan {
    case: CaseId,
    from: VersionRef,
    to: VersionRef,
    store_version: u64,
    current_node: String,
    target_node: String,
    node_changes: Vec<NodeChange>,
    variable_changes: Vec<VariableChange>,
    history_preserved: bool,
    plan_digest: String,
    migrated: CaseRecord,
}

impl MigrationPlan {
    /// The Case being migrated.
    #[must_use]
    pub fn case(&self) -> &CaseId {
        &self.case
    }

    /// The version the Case pins.
    #[must_use]
    pub fn from(&self) -> &VersionRef {
        &self.from
    }

    /// The target version.
    #[must_use]
    pub fn to(&self) -> &VersionRef {
        &self.to
    }

    /// The Case's store version when the plan was made.
    #[must_use]
    pub fn store_version(&self) -> u64 {
        self.store_version
    }

    /// The current node under its old id.
    #[must_use]
    pub fn current_node(&self) -> &str {
        &self.current_node
    }

    /// The current node under the target id.
    #[must_use]
    pub fn target_node(&self) -> &str {
        &self.target_node
    }

    /// The nodes the migration re-points.
    #[must_use]
    pub fn node_changes(&self) -> &[NodeChange] {
        &self.node_changes
    }

    /// The target's variable contract and what happens to each path.
    #[must_use]
    pub fn variable_changes(&self) -> &[VariableChange] {
        &self.variable_changes
    }

    /// Whether completed-node history stays exactly as stored.
    #[must_use]
    pub fn history_preserved(&self) -> bool {
        self.history_preserved
    }

    /// The digest over the plan's canonical form.
    #[must_use]
    pub fn plan_digest(&self) -> &str {
        &self.plan_digest
    }

    /// The exact record applying the plan stores.
    #[must_use]
    pub fn migrated(&self) -> &CaseRecord {
        &self.migrated
    }

    /// The dry-run report.
    #[must_use]
    pub fn dry_run(&self) -> DryRunReport {
        DryRunReport {
            case: self.case.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            current_node: self.current_node.clone(),
            target_node: self.target_node.clone(),
            node_changes: self.node_changes.clone(),
            variable_changes: self.variable_changes.clone(),
            history_preserved: self.history_preserved,
            plan_digest: self.plan_digest.clone(),
        }
    }

    /// A plain-text dry run for a person.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "Case {}: {} -> {}\n  current node: {} -> {}\n  history preserved: {}\n",
            self.case,
            self.from,
            self.to,
            self.current_node,
            self.target_node,
            self.history_preserved
        );
        for change in &self.node_changes {
            out.push_str(&format!("  node {} -> {}\n", change.from, change.to));
        }
        for change in &self.variable_changes {
            out.push_str(&format!(
                "  variable {}: {}\n",
                change.name,
                change.action.token()
            ));
        }
        out.push_str(&format!("  digest: {}\n", self.plan_digest));
        out
    }

    /// Applies the plan through `target`, which must store the Case and the audit event
    /// atomically.
    ///
    /// # Errors
    /// A [`VersioningError::StalePlan`] when the store moved since planning, or whatever the
    /// target reports.
    pub fn apply(
        &self,
        target: &dyn MigrationTarget,
        now: OffsetDateTime,
        actor: Option<&str>,
    ) -> Result<AppliedMigration> {
        let audit = MigrationAudit {
            topic: MIGRATION_AUDIT_TOPIC.to_owned(),
            case: self.case.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            plan_digest: self.plan_digest.clone(),
            actor: actor.map(str::to_owned),
            store_version: self.store_version,
            nodes_changed: self.node_changes.len(),
            variables_defaulted: self
                .variable_changes
                .iter()
                .filter(|change| matches!(change.action, VariableAction::Defaulted { .. }))
                .count(),
            at: now,
        };
        target.apply_case_migration(self, &audit)
    }
}

/// What applying a plan succeeded in doing.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedMigration {
    /// The Case.
    pub case: CaseId,
    /// Where it came from.
    pub from: VersionRef,
    /// Where it went.
    pub to: VersionRef,
    /// The plan's digest.
    pub plan_digest: String,
    /// The audit event written with it.
    pub audit: MigrationAudit,
}

/// Where a Case migration is stored.
///
/// The host implements this port over its transaction: [`MigrationPlan::migrated`] is the
/// record to write and [`MigrationAudit`] is the event to enqueue in the same transaction. A
/// store that cannot replace the pinned `dag_version` and `content_hash` of a run must not
/// pretend it can; this port is the seam (see the crate's open questions).
pub trait MigrationTarget: Send + Sync {
    /// Stores the migrated Case and its audit event atomically.
    ///
    /// # Errors
    /// [`VersioningError::StalePlan`] when the stored Case moved, or a store failure.
    fn apply_case_migration(
        &self,
        plan: &MigrationPlan,
        audit: &MigrationAudit,
    ) -> Result<AppliedMigration>;
}

/// The in-memory [`MigrationTarget`] the tests use.
#[derive(Debug, Default)]
pub struct MemoryMigrationTarget {
    inner: Mutex<TargetInner>,
}

#[derive(Debug, Default)]
struct TargetInner {
    records: BTreeMap<CaseId, CaseRecord>,
    audits: BTreeMap<String, MigrationAudit>,
}

impl MemoryMigrationTarget {
    /// An empty target.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A target already holding one Case.
    #[must_use]
    pub fn with_case(self, record: CaseRecord) -> Self {
        {
            let mut inner = self.lock();
            inner.records.insert(record.run.id.clone(), record);
        }
        self
    }

    fn lock(&self) -> MutexGuard<'_, TargetInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The stored Case, when the target holds it.
    #[must_use]
    pub fn load(&self, case: &CaseId) -> Option<CaseRecord> {
        self.lock().records.get(case).cloned()
    }

    /// Every audit event written, in digest order.
    #[must_use]
    pub fn audits(&self) -> Vec<MigrationAudit> {
        self.lock().audits.values().cloned().collect()
    }
}

impl MigrationTarget for MemoryMigrationTarget {
    fn apply_case_migration(
        &self,
        plan: &MigrationPlan,
        audit: &MigrationAudit,
    ) -> Result<AppliedMigration> {
        let mut inner = self.lock();
        if let Some(existing) = inner.audits.get(&audit.plan_digest) {
            return Ok(AppliedMigration {
                case: plan.case.clone(),
                from: plan.from.clone(),
                to: plan.to.clone(),
                plan_digest: audit.plan_digest.clone(),
                audit: existing.clone(),
            });
        }
        if let Some(current) = inner.records.get(&plan.case)
            && current.run.version != plan.store_version
        {
            return Err(VersioningError::StalePlan {
                case: plan.case.to_string(),
                planned: plan.store_version,
                current: current.run.version,
            });
        }
        let mut stored = plan.migrated.clone();
        stored.run.version = plan.store_version.saturating_add(1);
        inner.records.insert(plan.case.clone(), stored);
        inner
            .audits
            .insert(audit.plan_digest.clone(), audit.clone());
        Ok(AppliedMigration {
            case: plan.case.clone(),
            from: plan.from.clone(),
            to: plan.to.clone(),
            plan_digest: audit.plan_digest.clone(),
            audit: audit.clone(),
        })
    }
}

/// Plans the migration of one Case from `from` to `to`.
///
/// The Case must pin `from` exactly. Only nodes the Case can still run are re-pointed; completed
/// nodes keep their history. See the module docs for the refusals.
///
/// # Errors
/// [`VersioningError::VersionMismatch`], [`VersioningError::CaseNotRunning`],
/// [`VersioningError::UnmappedNode`], [`VersioningError::UnknownTargetNode`],
/// [`VersioningError::MappingCollision`] or [`VersioningError::VariableMissing`].
pub fn plan_migration(
    case: &CaseRecord,
    from: &ProcessDefinition,
    to: &ProcessDefinition,
    mapping: &MigrationMapping,
) -> Result<MigrationPlan> {
    let case_pin = VersionRef::of(case);
    let from_pin = VersionRef {
        name: from.name().to_owned(),
        version: from.number(),
        hash: from.compiled.dag.hash().to_string(),
    };
    if case.run.status != RunStatus::Running {
        return Err(VersioningError::CaseNotRunning {
            case: case.run.id.to_string(),
            status: status_token(case.run.status),
        });
    }
    if case_pin != from_pin || from.name() != to.name() {
        return Err(VersioningError::VersionMismatch {
            case: case.run.id.to_string(),
            expected: from_pin.to_string(),
            found: case_pin.to_string(),
        });
    }
    let to_pin = VersionRef {
        name: to.name().to_owned(),
        version: to.number(),
        hash: to.compiled.dag.hash().to_string(),
    };
    if to_pin == from_pin {
        return Err(VersioningError::VersionMismatch {
            case: case.run.id.to_string(),
            expected: to_pin.to_string(),
            found: from_pin.to_string(),
        });
    }

    let current: Vec<NodeId> = {
        let ready = case.run.ready_nodes();
        if ready.is_empty() {
            case.run
                .nodes
                .iter()
                .filter(|(_, state)| state.status == NodeStatus::Running)
                .map(|(node, _)| node.clone())
                .collect()
        } else {
            ready
        }
    };
    if current.is_empty() {
        return Err(VersioningError::CaseNotRunning {
            case: case.run.id.to_string(),
            status: "running with no current node".to_owned(),
        });
    }

    let mut remapped: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    let mut claimed: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    for (node, state) in &case.run.nodes {
        if state.status.is_terminal() {
            continue;
        }
        let target = resolve_node(case, mapping, to, node)?;
        // A runnable node must not land on the id of a node that already finished: that would
        // overwrite completed history and leave the Case on mixed versions.
        if case
            .run
            .nodes
            .iter()
            .any(|(done, state)| state.status.is_terminal() && done == &target)
        {
            return Err(VersioningError::MappingCollision {
                node: target.to_string(),
                first: target.to_string(),
                second: node.to_string(),
            });
        }
        if let Some(first) = claimed.insert(target.clone(), node.clone()) {
            return Err(VersioningError::MappingCollision {
                node: target.to_string(),
                first: first.to_string(),
                second: node.to_string(),
            });
        }
        remapped.insert(node.clone(), target);
    }

    let mut run = case.run.clone();
    run.dag_key = to.name().to_owned();
    run.dag_version = to.number();
    run.content_hash = to_pin.hash.clone();
    let mut nodes = BTreeMap::new();
    for (node, state) in &case.run.nodes {
        let key = remapped.get(node).cloned().unwrap_or_else(|| node.clone());
        nodes.insert(key, state.clone());
    }
    run.nodes = nodes;

    let mut attempts = BTreeMap::new();
    for (node, attempt) in &case.attempts {
        let key = remapped.get(node).cloned().unwrap_or_else(|| node.clone());
        let mut attempt = attempt.clone();
        attempt.node = key.clone();
        attempts.insert(key, attempt);
    }
    let mut timers = BTreeMap::new();
    for (node, timer) in &case.timers {
        if let Some(key) = remapped.get(node) {
            let mut timer = timer.clone();
            timer.node = key.clone();
            timers.insert(key.clone(), timer);
        } else {
            timers.insert(node.clone(), timer.clone());
        }
    }
    run.taken = case
        .run
        .taken
        .iter()
        .map(|edge| TakenEdge {
            from: remapped
                .get(&edge.from)
                .cloned()
                .unwrap_or_else(|| edge.from.clone()),
            to: remapped
                .get(&edge.to)
                .cloned()
                .unwrap_or_else(|| edge.to.clone()),
        })
        .collect();
    let mut children = BTreeMap::new();
    for (node, child) in &case.children {
        if let Some(key) = remapped.get(node) {
            let mut child = child.clone();
            child.node = key.clone();
            children.insert(key.clone(), child);
        } else {
            children.insert(node.clone(), child.clone());
        }
    }

    let mut variables = case.variables.clone();
    if !variables.is_object() {
        variables = Json::Object(serde_json::Map::new());
    }
    let mut variable_changes = Vec::new();
    for path in mapping.required() {
        if variable_at(&variables, path).is_some() {
            variable_changes.push(VariableChange {
                name: path.clone(),
                action: VariableAction::Kept,
            });
        } else if let Some(default) = mapping.defaults().get(path) {
            set_variable(&mut variables, path, default.clone());
            variable_changes.push(VariableChange {
                name: path.clone(),
                action: VariableAction::Defaulted {
                    value: default.clone(),
                },
            });
        } else {
            return Err(VersioningError::VariableMissing {
                variable: path.clone(),
            });
        }
    }

    let history_preserved = case.run.nodes.iter().all(|(node, state)| {
        !state.status.is_terminal() || run.nodes.get(node).is_some_and(|stored| stored == state)
    });

    let current_node = match current.first() {
        Some(node) => node.clone(),
        None => {
            return Err(VersioningError::CaseNotRunning {
                case: case.run.id.to_string(),
                status: "running with no current node".to_owned(),
            });
        }
    };
    let target_node = remapped
        .get(&current_node)
        .cloned()
        .unwrap_or_else(|| current_node.clone());
    let node_changes: Vec<NodeChange> = remapped
        .iter()
        .filter(|(from, to)| from != to)
        .map(|(from, to)| NodeChange {
            from: from.to_string(),
            to: to.to_string(),
        })
        .collect();

    let mut migrated = case.clone();
    migrated.run = run;
    migrated.variables = variables;
    migrated.attempts = attempts;
    migrated.timers = timers;
    migrated.children = children;

    let mut plan = MigrationPlan {
        case: case.run.id.clone(),
        from: from_pin,
        to: to_pin,
        store_version: case.run.version,
        current_node: current_node.to_string(),
        target_node: target_node.to_string(),
        node_changes,
        variable_changes,
        history_preserved,
        plan_digest: String::new(),
        migrated,
    };
    plan.plan_digest = digest_of(&plan);
    Ok(plan)
}

fn resolve_node(
    case: &CaseRecord,
    mapping: &MigrationMapping,
    to: &ProcessDefinition,
    node: &NodeId,
) -> Result<NodeId> {
    if let Some(target) = mapping.target_of(node) {
        if to.node(target).is_none() {
            return Err(VersioningError::UnknownTargetNode {
                node: target.to_string(),
            });
        }
        return Ok(target.clone());
    }
    if to.node(node).is_some() {
        return Ok(node.clone());
    }
    Err(VersioningError::UnmappedNode {
        case: case.run.id.to_string(),
        node: node.to_string(),
    })
}

fn status_token(status: RunStatus) -> String {
    match status {
        RunStatus::Running => "running",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
    .to_owned()
}

fn variable_at<'a>(root: &'a Json, path: &str) -> Option<&'a Json> {
    let mut current = root;
    for part in path.split('.') {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

fn set_variable(root: &mut Json, path: &str, value: Json) {
    fn at(current: &mut Json, parts: &[&str], value: Json) {
        let Some((head, rest)) = parts.split_first() else {
            *current = value;
            return;
        };
        if !current.is_object() {
            *current = Json::Object(serde_json::Map::new());
        }
        if let Json::Object(map) = current {
            let child = map.entry((*head).to_owned()).or_insert(Json::Null);
            at(child, rest, value);
        }
    }
    let parts: Vec<&str> = path.split('.').collect();
    at(root, &parts, value);
}

#[derive(Serialize)]
struct Canonical<'a> {
    case: &'a CaseId,
    from: &'a VersionRef,
    to: &'a VersionRef,
    current: &'a str,
    target: &'a str,
    nodes: &'a [NodeChange],
    variables: &'a [VariableChange],
    history: bool,
    run: &'a spot_dag_exec::Run,
    attempts: &'a BTreeMap<NodeId, spot_bpm_engine::Attempt>,
    timers: &'a BTreeMap<NodeId, spot_bpm_engine::TimerRecord>,
    children: &'a BTreeMap<NodeId, spot_bpm_engine::ChildLink>,
    initiator: &'a Option<String>,
}

fn digest_of(plan: &MigrationPlan) -> String {
    let canonical = Canonical {
        case: &plan.case,
        from: &plan.from,
        to: &plan.to,
        current: &plan.current_node,
        target: &plan.target_node,
        nodes: &plan.node_changes,
        variables: &plan.variable_changes,
        history: plan.history_preserved,
        run: &plan.migrated.run,
        attempts: &plan.migrated.attempts,
        timers: &plan.migrated.timers,
        children: &plan.migrated.children,
        initiator: &plan.migrated.initiator,
    };
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn variables_read_and_write_at_dotted_paths() {
        let mut root = Json::Object(serde_json::Map::new());
        set_variable(&mut root, "order.total", Json::from(7));
        assert_eq!(variable_at(&root, "order.total"), Some(&Json::from(7)));
        assert_eq!(variable_at(&root, "order.missing"), None);
    }

    #[test]
    fn a_status_token_is_plain() {
        assert_eq!(status_token(RunStatus::Cancelled), "cancelled");
    }
}
