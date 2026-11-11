//! The node-handler contract: what a node is, what it returns and the registry that resolves it.
//!
//! A node handler is a small, synchronous, pure function of its [`NodeCtx`]: it reads the node's
//! config, the Case's variables and the time it was handed, and returns a [`NodeOutcome`]. It
//! never touches storage, never reads a clock and never blocks — the engine persists what it
//! returns. A handler that performs a side effect must key that effect by [`NodeCtx::key`], the
//! `(case, node, attempt)` idempotency key, so a replay after a crash is harmless.
//!
//! The registry is open: Document, Decision and Integration handlers are registered by
//! WP-BPM-04/05/06 without changing this crate. [`HandlerRegistry::logic_default`] is the set
//! this crate ships (Logic nodes, the Trigger pass-through and the Wait handler).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_model::{LogicType, NodeKind as ModelNodeKind};
use spot_dag::NodeId;

use crate::error::EngineError;

/// The identifier of one Case: the id of a run of the DAG execution layer.
///
/// A Case is one journey through a Process, and it *is* a `spot-dag-exec` run — same grammar
/// (`[a-z][a-z0-9_]{0,63}`), same store. The engine adds the Case-level bookkeeping the DAG layer
/// does not carry (attempt outcomes, timers, child links, variables) around the same run.
pub use spot_dag_exec::RunId as CaseId;

/// What a Logic node does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicKind {
    /// A conditional branch: the true outgoing conditions are taken.
    Branch,
    /// A parallel split: every outgoing branch is taken.
    Parallel,
    /// A join of parallel paths: the DAG layer's join rule does the waiting.
    Join,
    /// A bounded loop over the Case's variables.
    Loop,
    /// Data mapping with the shared expression language.
    Map,
    /// A TypeScript code node (its host is WP-BPM-06).
    Script,
}

impl LogicKind {
    /// The token this kind reports and the model spells.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Parallel => "parallel",
            Self::Join => "join",
            Self::Loop => "loop",
            Self::Map => "map",
            Self::Script => "script",
        }
    }
}

/// The execution kind of a node: what the engine needs to pick a handler.
///
/// It mirrors the Process model's [`ModelNodeKind`] but is its own type so the registry is open:
/// a kind this crate does not handle is [`NodeKind::Unknown`] and is reported, never a panic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Starts a Case (the entry node).
    Trigger,
    /// Creates, maps, updates, posts or cancels a document (WP-BPM-04).
    Document,
    /// A person decides (WP-BPM-05).
    Decision,
    /// A connector call (WP-BPM-06).
    Integration,
    /// A logic step: branch, parallel, join, loop, map or script.
    Logic(LogicKind),
    /// Pauses for an event, a date or a duration.
    Wait,
    /// Runs another Process inside this one.
    SubProcess,
    /// An AI agent step.
    Agent,
    /// A kind this build does not handle; the token is kept verbatim.
    Unknown(String),
}

impl NodeKind {
    /// The execution kind of a model node kind.
    #[must_use]
    pub fn from_model(kind: &ModelNodeKind) -> Self {
        match kind {
            ModelNodeKind::Trigger(_) => Self::Trigger,
            ModelNodeKind::Document(_) => Self::Document,
            ModelNodeKind::Decision(_) => Self::Decision,
            ModelNodeKind::Integration(_) => Self::Integration,
            ModelNodeKind::Logic(config) => Self::Logic(match config.logic {
                LogicType::Branch => LogicKind::Branch,
                LogicType::Parallel => LogicKind::Parallel,
                LogicType::Join => LogicKind::Join,
                LogicType::Loop => LogicKind::Loop,
                LogicType::Map => LogicKind::Map,
                LogicType::Script => LogicKind::Script,
            }),
            ModelNodeKind::Wait(_) => Self::Wait,
            ModelNodeKind::SubProcess(_) => Self::SubProcess,
            ModelNodeKind::Agent(_) => Self::Agent,
            ModelNodeKind::Unknown(raw) => Self::Unknown(raw.kind.clone()),
            // `ModelNodeKind` is `#[non_exhaustive]`: a kind a later work package adds is
            // preserved as unknown rather than refused.
            _ => Self::Unknown("unknown".to_owned()),
        }
    }

    /// The token for logs and error messages.
    #[must_use]
    pub fn token(&self) -> &str {
        match self {
            Self::Trigger => "trigger",
            Self::Document => "document",
            Self::Decision => "decision",
            Self::Integration => "integration",
            Self::Logic(kind) => kind.token(),
            Self::Wait => "wait",
            Self::SubProcess => "sub_process",
            Self::Agent => "agent",
            Self::Unknown(raw) => raw,
        }
    }
}

/// The `(case, node, attempt)` key a side-effecting handler keys its effect by.
///
/// It is the same key the DAG execution store deduplicates a transition by, widened with the
/// engine's attempt number so a retry gets a fresh key and a replay of one attempt does not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct IdempotencyKey {
    /// The Case the node belongs to.
    pub case: CaseId,
    /// The node being run.
    pub node: NodeId,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
}

impl IdempotencyKey {
    /// Builds the key.
    #[must_use]
    pub fn new(case: CaseId, node: NodeId, attempt: u32) -> Self {
        Self {
            case,
            node,
            attempt,
        }
    }
}

impl core::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}:{}", self.case, self.node, self.attempt)
    }
}

/// What a `Wait` node waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitSpec {
    /// Wait until a due instant.
    Until {
        /// The instant the wait ends.
        at: OffsetDateTime,
    },
    /// Wait for an event, optionally with a timeout.
    ForEvent {
        /// The event key a signal must carry.
        key: String,
        /// The instant the wait times out, when it has one.
        due: Option<OffsetDateTime>,
    },
    /// Wait for a person, optionally with a deadline.
    HumanTask {
        /// The task key a signal must carry.
        key: String,
        /// The instant the task times out, when it has one.
        due: Option<OffsetDateTime>,
    },
}

impl WaitSpec {
    /// The instant this wait becomes due, when it has one.
    #[must_use]
    pub fn due(&self) -> Option<OffsetDateTime> {
        match self {
            Self::Until { at } => Some(*at),
            Self::ForEvent { due, .. } | Self::HumanTask { due, .. } => *due,
        }
    }

    /// The event or task key a signal must match, when the wait needs one.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Until { .. } => None,
            Self::ForEvent { key, .. } | Self::HumanTask { key, .. } => Some(key),
        }
    }
}

/// What one node attempt did, as its handler reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeOutcome {
    /// The attempt succeeded. `branches` names the outgoing edge conditions to take.
    Done {
        /// The node's output payload, canonical JSON with no floats.
        output: Option<Json>,
        /// The condition labels the node chose.
        branches: Vec<String>,
    },
    /// The node pauses; the engine registers a timer and completes it later.
    Wait(WaitSpec),
    /// The node asks to be retried at a later instant.
    Retry {
        /// When to retry.
        after: OffsetDateTime,
    },
    /// The node failed. The engine retries per policy, then takes the error branch.
    Fail {
        /// A stable code naming the failure.
        code: String,
        /// A plain-language reason; never names a secret.
        reason: String,
    },
}

impl NodeOutcome {
    /// A success with no output and no branch.
    #[must_use]
    pub fn done() -> Self {
        Self::Done {
            output: None,
            branches: Vec::new(),
        }
    }

    /// A success that takes the named branches.
    #[must_use]
    pub fn done_with(branches: Vec<String>) -> Self {
        Self::Done {
            output: None,
            branches,
        }
    }

    /// A failure with a code and a reason.
    #[must_use]
    pub fn fail(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Fail {
            code: code.into(),
            reason: reason.into(),
        }
    }
}

/// The reserved condition the Process compiler gives an error branch.
pub const ERROR_CONDITION: &str = "error";

/// One outgoing edge of the node being run, as the handler sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingEdge {
    /// The node the edge points at.
    pub to: NodeId,
    /// The edge's condition; `None` for an unconditional edge, always taken.
    pub condition: Option<String>,
    /// Whether this is the node's error branch, taken only after the last retry.
    pub error: bool,
}

/// Everything a handler reads and may change for one attempt.
///
/// The engine owns the Case's variables and hands them in mutably: a handler that maps data
/// writes into `variables`, and the engine persists the result with the attempt. The
/// [`NodeCtx::key`] is the idempotency key a side-effecting handler must use.
pub struct NodeCtx<'a> {
    /// The Case being run.
    pub case: &'a CaseId,
    /// The node being run.
    pub node: &'a NodeId,
    /// The node's execution kind.
    pub kind: NodeKind,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
    /// The `(case, node, attempt)` idempotency key.
    pub key: IdempotencyKey,
    /// Whether this run replays a started-but-unfinished attempt after a crash.
    pub is_retry: bool,
    /// The instant the engine was stepped at.
    pub now: OffsetDateTime,
    /// The instant the attempt times out, when the node sets a timeout.
    pub deadline: Option<OffsetDateTime>,
    /// The principal the node runs as, when the Case carries one.
    pub principal: Option<&'a str>,
    /// The node's config, as compiled from the Process model.
    pub config: &'a Json,
    /// The node's outgoing edges, so a branch handler can name the edge it chooses.
    pub outgoing: &'a [OutgoingEdge],
    /// The Case's variables; a handler may write mapped values here.
    pub variables: &'a mut Json,
    /// Whether the Case has been cancelled.
    pub cancelled: bool,
}

impl NodeCtx<'_> {
    /// The value at a dotted path inside the Case's variables.
    #[must_use]
    pub fn variable(&self, path: &str) -> Option<&Json> {
        let mut current = &*self.variables;
        for part in path.split('.') {
            current = current.as_object()?.get(part)?;
        }
        Some(current)
    }

    /// Sets the value at a dotted path inside the Case's variables, creating objects.
    pub fn set_variable(&mut self, path: &str, value: Json) {
        let parts: Vec<&str> = path.split('.').collect();
        set_at(self.variables, &parts, value);
    }

    /// The conditions of the node's non-error outgoing edges, in edge order.
    #[must_use]
    pub fn outgoing_conditions(&self) -> Vec<String> {
        self.outgoing
            .iter()
            .filter(|edge| !edge.error)
            .filter_map(|edge| edge.condition.clone())
            .collect()
    }
}

/// Writes `value` at the dotted path `parts` inside `current`, creating objects on the way.
fn set_at(current: &mut Json, parts: &[&str], value: Json) {
    let Some((head, rest)) = parts.split_first() else {
        return;
    };
    if !current.is_object() {
        *current = Json::Object(serde_json::Map::new());
    }
    let Json::Object(map) = current else {
        return;
    };
    if rest.is_empty() {
        map.insert((*head).to_owned(), value);
        return;
    }
    let child = map
        .entry((*head).to_owned())
        .or_insert_with(|| Json::Object(serde_json::Map::new()));
    set_at(child, rest, value);
}

/// One node handler: the engine's unit of work.
///
/// Handlers are synchronous and pure; a handler that must call the outside world does so
/// through the host's ports, keyed by [`NodeCtx::key`].
pub trait NodeHandler: Send + Sync {
    /// The node kind this handler runs.
    fn kind(&self) -> NodeKind;

    /// Runs one attempt and reports its outcome.
    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome;
}

/// The open registry of node handlers, keyed by [`NodeKind`].
#[derive(Default)]
pub struct HandlerRegistry {
    handlers: BTreeMap<NodeKind, Arc<dyn NodeHandler>>,
}

impl HandlerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler, replacing any handler already registered for its kind.
    pub fn register(&mut self, handler: impl NodeHandler + 'static) -> &mut Self {
        self.handlers.insert(handler.kind(), Arc::new(handler));
        self
    }

    /// Registers a boxed handler.
    pub fn register_arc(&mut self, handler: Arc<dyn NodeHandler>) -> &mut Self {
        self.handlers.insert(handler.kind(), handler);
        self
    }

    /// The handler for a kind, when one is registered.
    #[must_use]
    pub fn get(&self, kind: &NodeKind) -> Option<Arc<dyn NodeHandler>> {
        self.handlers.get(kind).cloned()
    }

    /// Whether a handler is registered for a kind.
    #[must_use]
    pub fn contains(&self, kind: &NodeKind) -> bool {
        self.handlers.contains_key(kind)
    }

    /// The kinds that have a handler.
    #[must_use]
    pub fn kinds(&self) -> Vec<NodeKind> {
        self.handlers.keys().cloned().collect()
    }

    /// The handlers this crate ships: Logic nodes, the Trigger pass-through and the Wait handler.
    #[must_use]
    pub fn logic_default() -> Self {
        let mut registry = Self::new();
        crate::logic::register(&mut registry);
        registry.register(TriggerHandler);
        registry.register(crate::logic::WaitHandler);
        registry
    }
}

/// The Trigger handler: starting the Case *is* the trigger, so it is a pass-through.
#[derive(Debug, Clone, Copy, Default)]
pub struct TriggerHandler;

impl NodeHandler for TriggerHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Trigger
    }

    fn run(&self, _ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        NodeOutcome::done()
    }
}

/// Resolves a node's execution kind from the pinned process's side table.
///
/// # Errors
/// [`EngineError::HandlerMissing`] is not raised here; a kind that has no entry is
/// [`NodeKind::Unknown`] with the model's token, which the engine then reports.
#[must_use]
pub fn node_kind(process: &spot_bpm_model::CompiledProcess, node: &NodeId) -> NodeKind {
    let model_id = spot_bpm_model::NodeId::new(node.as_str());
    match model_id.ok().and_then(|id| process.nodes.get(&id)) {
        Some(kind) => NodeKind::from_model(kind),
        None => NodeKind::Unknown("unknown".to_owned()),
    }
}

/// The node config as JSON, from the compiled DAG node.
#[must_use]
pub fn node_config(process: &spot_bpm_model::CompiledProcess, node: &NodeId) -> Json {
    process
        .dag
        .node(node)
        .map(|dag_node| dag_node.config.clone())
        .unwrap_or(Json::Null)
}

/// The node's outgoing edges, as the handler sees them.
#[must_use]
pub fn outgoing_edges(
    process: &spot_bpm_model::CompiledProcess,
    node: &NodeId,
) -> Vec<OutgoingEdge> {
    process
        .dag
        .successors(node)
        .iter()
        .map(|edge| OutgoingEdge {
            to: edge.node.clone(),
            error: edge.condition.as_deref() == Some(ERROR_CONDITION),
            condition: edge.condition.clone(),
        })
        .collect()
}

/// A helper for handlers that must report a malformed config.
///
/// # Errors
/// Always [`EngineError::ConfigInvalid`].
pub fn config_error(node: &NodeId, detail: impl Into<String>) -> EngineError {
    EngineError::ConfigInvalid {
        node: node.to_string(),
        detail: detail.into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn logic_kinds_have_tokens() {
        assert_eq!(LogicKind::Branch.token(), "branch");
        assert_eq!(LogicKind::Script.token(), "script");
    }

    #[test]
    fn idempotency_keys_render() {
        let key = IdempotencyKey::new(
            CaseId::new("case1").unwrap(),
            NodeId::new("node1").unwrap(),
            3,
        );
        assert_eq!(key.to_string(), "case1:node1:3");
    }

    #[test]
    fn variables_read_and_write_at_dotted_paths() {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("node1").unwrap();
        let mut variables = Json::Object(serde_json::Map::new());
        let outgoing = Vec::new();
        let config = Json::Null;
        let mut ctx = NodeCtx {
            case: &case,
            node: &node,
            kind: NodeKind::Logic(LogicKind::Map),
            attempt: 1,
            key: IdempotencyKey::new(case.clone(), node.clone(), 1),
            is_retry: false,
            now: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            deadline: None,
            principal: None,
            config: &config,
            outgoing: &outgoing,
            variables: &mut variables,
            cancelled: false,
        };
        ctx.set_variable("order.total", Json::from(5));
        assert_eq!(ctx.variable("order.total"), Some(&Json::from(5)));
        assert_eq!(ctx.variable("order.missing"), None);
    }
}
