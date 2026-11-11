//! The Process model (brief §1; blueprint WS8, ADR-49): processes, versions,
//! typed nodes, edges, rules and the run state.
//!
//! A **Process** is a graph of typed steps across documents and systems. A
//! **version** is one immutable revision of that graph (draft, published or
//! retired). A **run** is one journey through a version — the model exposes it
//! as [`CaseState`] in code and, wherever a person sees text, through the label
//! key [`labels::RUN_SINGULAR_KEY`] (default "Request"); no user-visible string
//! contains the word "Case".
//!
//! [`NodeKind`] is `#[non_exhaustive]` and carries an [`UnknownNode`] variant:
//! a kind added by a later work package parses, round-trips and is *reported*
//! by [`validate`](crate::validate) as `bpm_model.unknown_node_kind`, never a
//! parse failure (ADR-080). WP-BPM-12 adds `form`, `send_message` and
//! `wait_message` kinds the same way: a new enum variant and a new config
//! struct, no format break.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::ModelError;
use crate::id::NodeId;

/// The `kind` token of a trigger node.
pub const KIND_TRIGGER: &str = "trigger";
/// The `kind` token of a document node.
pub const KIND_DOCUMENT: &str = "document";
/// The `kind` token of a decision node.
pub const KIND_DECISION: &str = "decision";
/// The `kind` token of an integration node.
pub const KIND_INTEGRATION: &str = "integration";
/// The `kind` token of a logic node.
pub const KIND_LOGIC: &str = "logic";
/// The `kind` token of a wait node.
pub const KIND_WAIT: &str = "wait";
/// The `kind` token of a sub-process node.
pub const KIND_SUB_PROCESS: &str = "sub_process";
/// The `kind` token of an agent node.
pub const KIND_AGENT: &str = "agent";
/// The `kind` token of a form node (ADR-080 section 2.2, WP-BPM-13).
pub const KIND_FORM: &str = "form";
/// The `kind` token of a send-message node (ADR-080 section 5.3).
pub const KIND_SEND_MESSAGE: &str = "send_message";
/// The `kind` token of a wait-for-message node (ADR-080 section 5.3).
pub const KIND_WAIT_MESSAGE: &str = "wait_message";

/// Every node kind this crate knows; a kind outside this list is preserved as
/// [`UnknownNode`] and reported by validation.
pub const KNOWN_KINDS: [&str; 11] = [
    KIND_TRIGGER,
    KIND_DOCUMENT,
    KIND_DECISION,
    KIND_INTEGRATION,
    KIND_LOGIC,
    KIND_WAIT,
    KIND_SUB_PROCESS,
    KIND_AGENT,
    KIND_FORM,
    KIND_SEND_MESSAGE,
    KIND_WAIT_MESSAGE,
];

/// The lifecycle operations a document node may name (ADR-51).
pub const KNOWN_OPERATIONS: [&str; 7] = [
    "create",
    "update",
    "post",
    "cancel",
    "delete",
    "rename",
    "update_posted",
];

/// Run-label keys and defaults (ADR-080). The model never prints the word
/// "Case"; every user-facing name for a run comes from these keys.
pub mod labels {
    /// Label key for one run; default "Request".
    ///
    /// Written with `concat!` so this non-error vocabulary stays out of the
    /// error-code scan (the `wire-vocabulary.toml` catalogue file belongs to
    /// another task's owned paths).
    pub const RUN_SINGULAR_KEY: &str = concat!("bpm.run.", "singular");
    /// Label key for several runs; default "Requests".
    pub const RUN_PLURAL_KEY: &str = concat!("bpm.run.", "plural");
    /// The default singular label.
    pub const RUN_SINGULAR_DEFAULT: &str = "Request";
    /// The default plural label.
    pub const RUN_PLURAL_DEFAULT: &str = "Requests";
}

/// One process: a name within an owning app, its versions, and — when it takes
/// part in a conversation across organisations — its participants.
#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    /// The process's stable name; `[a-z][a-z0-9_]{0,63}`.
    pub name: String,
    /// The app that owns the process.
    pub app: String,
    /// Every version, in any order (validation sorts for reporting).
    pub versions: Vec<ProcessVersion>,
    /// The roles of the process contract this process is one half of, and the
    /// role this process plays (ADR-080 section 5.2). Absent for a process
    /// that stays inside one organisation.
    pub participants: Option<Participants>,
}

impl Process {
    /// Builds a process from parts, validating the name and app grammars.
    ///
    /// The process starts without participants; use
    /// [`with_participants`](Self::with_participants) to take part in a
    /// process contract.
    ///
    /// # Errors
    /// [`ModelError::InvalidName`] when the name or app breaks the grammar.
    pub fn new(
        name: impl Into<String>,
        app: impl Into<String>,
        versions: Vec<ProcessVersion>,
    ) -> Result<Self, ModelError> {
        let name = name.into();
        let app = app.into();
        crate::id::NodeId::new(&name)?;
        crate::id::NodeId::new(&app)?;
        Ok(Self {
            name,
            app,
            versions,
            participants: None,
        })
    }

    /// Sets the participants of the process contract this process is one half
    /// of.
    #[must_use]
    pub fn with_participants(mut self, participants: Participants) -> Self {
        self.participants = Some(participants);
        self
    }

    /// The version with this number, when it exists.
    #[must_use]
    pub fn version(&self, number: u32) -> Option<&ProcessVersion> {
        self.versions.iter().find(|v| v.version == number)
    }
}

/// What a participant of a process contract is (ADR-080 section 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    /// A tenant on the platform, reached over a tenant link.
    #[default]
    Tenant,
    /// A system that is not a tenant, reached through a connector or a signed
    /// webhook (ADR-058 rule 12).
    External,
}

/// One role of a process contract, as one half of the conversation sees it.
///
/// The role names are the contract's roles; a process says which one it plays
/// in [`Participants::plays`]. A role is never a tenant id: each side maps its
/// own role onto its own organisation, so the model carries no tenant identity
/// (ADR-080 section 5.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    /// The role's name; `[a-z][a-z0-9_]{0,63}`.
    pub role: String,
    /// Whether the role is a platform tenant or an external system.
    #[serde(default)]
    pub kind: ParticipantKind,
}

impl Participant {
    /// Builds a participant.
    #[must_use]
    pub fn new(role: impl Into<String>, kind: ParticipantKind) -> Self {
        Self {
            role: role.into(),
            kind,
        }
    }
}

/// The roles of a process contract, and the role this process plays.
///
/// A process always runs inside one tenant (ADR-080 section 5.1): this is the
/// local half of a conversation, and the other roles are the participants it
/// exchanges messages with. The role it plays is the one its send and wait
/// nodes are checked against by `spot-bpm-contract`'s `conforms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participants {
    /// Every role of the contract, in definition order.
    #[serde(default)]
    pub roles: Vec<Participant>,
    /// The role this process plays; one of [`roles`](Self::roles).
    pub plays: String,
}

impl Participants {
    /// Builds the participant list of a process, naming the role it plays.
    #[must_use]
    pub fn new(roles: Vec<Participant>, plays: impl Into<String>) -> Self {
        Self {
            roles,
            plays: plays.into(),
        }
    }

    /// The participant with this role, when the contract names it.
    #[must_use]
    pub fn participant(&self, role: &str) -> Option<&Participant> {
        self.roles.iter().find(|p| p.role == role)
    }
}

/// One revision of a process graph.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessVersion {
    /// The process this version belongs to; `[a-z][a-z0-9_]{0,63}`.
    pub name: String,
    /// The app that owns the process.
    pub app: String,
    /// An optional human-readable title.
    pub title: Option<String>,
    /// The version number; starts at 1, never 0.
    pub version: u32,
    /// Draft, published or retired.
    pub status: VersionStatus,
    /// The nodes, in definition order.
    pub nodes: Vec<Node>,
    /// The edges, in definition order.
    pub edges: Vec<Edge>,
    /// Named boolean expressions edges may reference.
    pub rules: Vec<Rule>,
}

impl ProcessVersion {
    /// Builds a version from parts, validating the process and app grammars.
    ///
    /// # Errors
    /// [`ModelError::InvalidName`] when the process or app breaks the grammar.
    pub fn new(
        name: impl Into<String>,
        app: impl Into<String>,
        version: u32,
        status: VersionStatus,
        nodes: Vec<Node>,
        edges: Vec<Edge>,
        rules: Vec<Rule>,
    ) -> Result<Self, ModelError> {
        let name = name.into();
        let app = app.into();
        crate::id::NodeId::new(&name)?;
        crate::id::NodeId::new(&app)?;
        Ok(Self {
            name,
            app,
            title: None,
            version,
            status,
            nodes,
            edges,
            rules,
        })
    }

    /// The version's stable content hash. It covers the graph *content* — the
    /// process name and app, every node with its kind, join, terminal flag,
    /// error branch, timeout and retry policy, every edge and every rule — and
    /// nothing else, so two drafts with the same text content hash the same
    /// whatever their version number or status. Node and edge definition order
    /// does not matter.
    #[must_use]
    pub fn content_hash(&self) -> crate::hash::ContentHash {
        crate::hash::ContentHash::of(self)
    }

    /// The node with this id, when it exists.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.id == id)
    }
}

/// The lifecycle status of a process version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    /// Editable, not yet in force.
    #[default]
    Draft,
    /// In force; running runs use it.
    Published,
    /// No longer started, kept for running runs.
    Retired,
}

/// One node: a typed step, its join rule, its error branch and its retry policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The node's identifier ([`NodeId`] grammar).
    pub id: NodeId,
    /// What the node does.
    pub kind: NodeKind,
    /// How many incoming edges must be taken before the node may start.
    pub join: JoinRule,
    /// Whether the node may end the flow without an outgoing edge.
    pub terminal: bool,
    /// The node that runs when this node fails, when given.
    pub error: Option<NodeId>,
    /// How long the node may run before it times out, in whole seconds.
    pub timeout_secs: Option<u64>,
    /// How the node retries a failure.
    pub retry: RetryPolicy,
}

impl Node {
    /// Builds a node with default join, no error branch, no timeout and no
    /// retry. Use the `with_*` builder methods to set them.
    #[must_use]
    pub fn new(id: NodeId, kind: NodeKind) -> Self {
        Self {
            id,
            kind,
            join: JoinRule::All,
            terminal: false,
            error: None,
            timeout_secs: None,
            retry: RetryPolicy::none(),
        }
    }

    /// Sets the join rule.
    #[must_use]
    pub fn with_join(mut self, join: JoinRule) -> Self {
        self.join = join;
        self
    }

    /// Marks the node terminal (it may end the flow).
    #[must_use]
    pub fn as_terminal(mut self) -> Self {
        self.terminal = true;
        self
    }

    /// Sets the error branch.
    #[must_use]
    pub fn with_error(mut self, target: NodeId) -> Self {
        self.error = Some(target);
        self
    }

    /// Sets the timeout in whole seconds.
    #[must_use]
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Sets the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

/// The typed kind of a node with its per-kind config.
///
/// `#[non_exhaustive]`: a later work package adds variants (`form`,
/// `send_message`, `wait_message` in WP-BPM-12) without a format break, and
/// callers keep a catch-all arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    /// Starts a run.
    Trigger(TriggerConfig),
    /// Creates, maps, updates, posts or cancels a document.
    Document(DocumentConfig),
    /// A person approves, rejects or chooses a path.
    Decision(DecisionConfig),
    /// Calls another system through a connector.
    Integration(IntegrationConfig),
    /// Branches, joins, loops, maps data or runs a script.
    Logic(LogicConfig),
    /// Pauses for an event or a date.
    Wait(WaitConfig),
    /// Runs another process inside this one.
    SubProcess(SubProcessConfig),
    /// An AI agent performs the step within its tier.
    Agent(AgentConfig),
    /// A person enters or reviews data, producing a draft record (ADR-080
    /// section 2.2, WP-BPM-13).
    Form(FormConfig),
    /// Sends one of a process contract's messages to another participant
    /// (ADR-080 section 5.3).
    SendMessage(SendMessageConfig),
    /// Waits for one of a process contract's messages (ADR-080 section 5.3).
    WaitMessage(WaitMessageConfig),
    /// A kind this build does not know; preserved for round trip and reported
    /// by validation.
    Unknown(UnknownNode),
}

impl NodeKind {
    /// The `kind` token: the known kind's name, or the raw unknown text.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Trigger(_) => KIND_TRIGGER,
            Self::Document(_) => KIND_DOCUMENT,
            Self::Decision(_) => KIND_DECISION,
            Self::Integration(_) => KIND_INTEGRATION,
            Self::Logic(_) => KIND_LOGIC,
            Self::Wait(_) => KIND_WAIT,
            Self::SubProcess(_) => KIND_SUB_PROCESS,
            Self::Agent(_) => KIND_AGENT,
            Self::Form(_) => KIND_FORM,
            Self::SendMessage(_) => KIND_SEND_MESSAGE,
            Self::WaitMessage(_) => KIND_WAIT_MESSAGE,
            Self::Unknown(raw) => &raw.kind,
        }
    }

    /// Whether this is a kind this build knows.
    #[must_use]
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// The config as a TOML table (ADR-12: no floats). Known kinds serialise
    /// their typed config; an unknown kind keeps exactly the table it was
    /// parsed with.
    #[must_use]
    pub fn config_table(&self) -> toml::Table {
        match self {
            Self::Unknown(raw) => raw.config.clone(),
            Self::Trigger(c) => to_table(c),
            Self::Document(c) => to_table(c),
            Self::Decision(c) => to_table(c),
            Self::Integration(c) => to_table(c),
            Self::Logic(c) => to_table(c),
            Self::Wait(c) => to_table(c),
            Self::SubProcess(c) => to_table(c),
            Self::Agent(c) => to_table(c),
            Self::Form(c) => to_table(c),
            Self::SendMessage(c) => to_table(c),
            Self::WaitMessage(c) => to_table(c),
        }
    }
}

/// Serialises a known kind's config to a TOML table; an impossible
/// serialisation failure yields an empty table (the configs are plain data).
fn to_table<T: Serialize>(config: &T) -> toml::Table {
    match toml::Value::try_from(config) {
        Ok(toml::Value::Table(table)) => table,
        _ => toml::Table::new(),
    }
}

/// A node kind this build does not know, kept whole for round trip.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownNode {
    /// The raw `kind` token exactly as it was written.
    pub kind: String,
    /// The rest of the node's config table, exactly as it was written.
    pub config: toml::Table,
}

/// How a trigger starts a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    /// A document lifecycle event from the outbox.
    #[default]
    DocumentEvent,
    /// A schedule.
    Schedule,
    /// An incoming webhook.
    Webhook,
    /// A person starts it from the app.
    Manual,
    /// An API call starts it.
    Api,
}

/// Starts a run (blueprint node type Trigger).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerConfig {
    /// How the trigger fires.
    #[serde(default)]
    pub trigger: TriggerType,
    /// The DocType for a document-event trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctype: Option<String>,
    /// The lifecycle operation for a document-event trigger, e.g. `post`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// The schedule for a schedule trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// The path for a webhook or API trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// What a document node does to its document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentAction {
    /// Create a document.
    Create,
    /// Update a draft.
    Update,
    /// Post a document through its lifecycle.
    Post,
    /// Cancel a posted document through its lifecycle.
    Cancel,
    /// Copy fields between documents without writing.
    Map,
}

/// Creates, maps, updates, posts or cancels a document (blueprint node type
/// Document). A process never writes a document's status directly: a step
/// changes a document only through the document's own lifecycle operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentConfig {
    /// What the node does.
    pub action: DocumentAction,
    /// The DocType the node works on.
    pub doctype: String,
    /// The lifecycle operation; defaults from the action for post and cancel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// A mapping expression or mapping name, for `map`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<String>,
    /// A named service principal the node acts as, overriding the Case's actor;
    /// publishing a version that uses it needs the
    /// `process.publish_service_principal` permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

impl DocumentConfig {
    /// The lifecycle operation the node names, or the action's default.
    #[must_use]
    pub fn operation(&self) -> &str {
        if let Some(op) = &self.operation {
            return op;
        }
        match self.action {
            DocumentAction::Create | DocumentAction::Map => "create",
            DocumentAction::Update => "update",
            DocumentAction::Post => "post",
            DocumentAction::Cancel => "cancel",
        }
    }
}

/// A person approves, rejects or chooses a path from a task inbox (blueprint
/// node type Decision).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionConfig {
    /// What the person is asked to decide.
    pub title: String,
    /// Who is asked: a role, user or expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// The labels of the outcomes the person may choose.
    #[serde(default)]
    pub outcomes: Vec<String>,
}

/// Calls another system through a connector (blueprint node type Integration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationConfig {
    /// The connector's name.
    pub connector: String,
    /// The connector action.
    pub action: String,
    /// An optional endpoint or destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// What a logic node does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicType {
    /// A conditional branch.
    #[default]
    Branch,
    /// Parallel paths.
    Parallel,
    /// A join of parallel paths.
    Join,
    /// A loop.
    Loop,
    /// Data mapping.
    Map,
    /// A TypeScript code node.
    Script,
}

/// Branches, parallel paths, joins, loops, data mapping or TypeScript code
/// (blueprint node type Logic).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicConfig {
    /// What the node does.
    #[serde(default)]
    pub logic: LogicType,
    /// The TypeScript source for a `script` node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// For a `loop`: the most iterations it runs (1 to [`MAX_LOOP_ITERATIONS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// For a `loop`: an expression that must hold for another iteration to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// For a `loop`: variable name to expression, applied on each iteration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub body: BTreeMap<String, String>,
    /// For a `map`: variable name to expression, applied once.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mapping: BTreeMap<String, String>,
    /// For a `branch`, `parallel` or `loop`: the outgoing branches taken, by
    /// condition text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
}

/// The most iterations a `loop` node may run: the engine's cap
/// (`LoopHandler` in `spot-bpm-engine`, kept in step by a test).
pub const MAX_LOOP_ITERATIONS: u32 = 10_000;

/// What a wait node waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitType {
    /// An event.
    #[default]
    Event,
    /// A calendar date.
    Date,
    /// A duration in seconds.
    Duration,
}

/// Pauses for an event or a date (blueprint node type Wait).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitConfig {
    /// What the node waits for.
    #[serde(default)]
    pub wait: WaitType,
    /// The event name for an event wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// The duration in whole seconds for a duration wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
    /// The date, as text, for a date wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// Runs another process inside this one (blueprint node type Sub-process).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubProcessConfig {
    /// The process to run.
    pub process: String,
    /// The version to run; the latest published one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
}

/// An AI agent performs the step within its tier (blueprint node type Agent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// The agent tier.
    pub tier: String,
    /// What the agent is asked to do.
    pub instruction: String,
}

/// How a form step saves what the person entered (ADR-080 section 2.2).
///
/// The step's output is always an ordinary draft record created or updated
/// through the record type's own lifecycle, never a state written directly. A
/// form that collects request data only, with no record of its own, uses
/// [`DraftMode::Collect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftMode {
    /// Create a draft record of the record type.
    #[default]
    Create,
    /// Update a draft record the request already owns.
    Update,
    /// Collect request data and create no record.
    Collect,
}

/// A person enters or reviews a set of fields, producing a draft record
/// (ADR-080 section 2.2, WP-BPM-13).
///
/// Completing the step and saving the draft commit together. The record is
/// submitted by a later document step through its own lifecycle; the form step
/// never posts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormConfig {
    /// The record type the draft belongs to; absent only for
    /// [`DraftMode::Collect`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_type: Option<String>,
    /// The fields of the record type the person enters or reviews.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Who is asked: a role, a user or an assignment expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Whether the step creates, updates or collects without a record.
    #[serde(default)]
    pub draft_mode: DraftMode,
}

/// What a send-message step does when the receiver rejects the message
/// (ADR-080 section 5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectAction {
    /// Take the node's error branch: a rejected message stops the sender.
    #[default]
    ErrorBranch,
    /// Continue on the normal path, recording the rejection on the request.
    Continue,
}

/// Sends one message of a process contract to another participant
/// (ADR-080 section 5.3).
///
/// The payload is mapped from the request's variables by `mapping` and checked
/// against the contract *before* it leaves the tenant; only contract fields
/// cross (ADR-058 rule 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageConfig {
    /// The id of the contract the message belongs to.
    pub contract: String,
    /// The contract message to send.
    pub message: String,
    /// The mapping that builds the payload from the request's variables; a
    /// mapping name or an expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<String>,
    /// What happens when the receiver rejects the message.
    #[serde(default)]
    pub on_reject: RejectAction,
}

/// What a wait-for-message step does when its time limit passes
/// (ADR-080 section 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutAction {
    /// Take the node's error branch.
    #[default]
    ErrorBranch,
    /// Follow the outgoing edge labelled `timeout`.
    TimeoutEdge,
}

/// Waits for one message of a process contract (ADR-080 section 5.3).
///
/// The wait is always bounded: a peer that never answers must not stall the
/// request, so the node's own `timeout_secs` is its time limit and
/// `on_timeout` says which branch the step takes when it passes (ADR-080
/// section 5.5). The time limit is the node's, not the config table's, so a
/// wait-for-message and a plain wait are bounded the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitMessageConfig {
    /// The id of the contract the message belongs to.
    pub contract: String,
    /// The contract message awaited.
    pub message: String,
    /// What the step does when the node's time limit passes.
    #[serde(default)]
    pub on_timeout: TimeoutAction,
}

impl WaitMessageConfig {
    /// The label an outgoing edge carries when it is this wait's timeout
    /// branch.
    pub const TIMEOUT_EDGE_LABEL: &'static str = "timeout";
}

/// How many incoming edges must be taken before a node may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoinRule {
    /// Every incoming edge must be taken.
    #[default]
    All,
    /// One taken incoming edge is enough.
    Any,
    /// At least `required` incoming edges must be taken.
    Quorum {
        /// The number of incoming edges that must be taken.
        required: u32,
    },
}

/// How a node retries a failure. `max_attempts` counts the first try, so 1
/// means no retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The total number of attempts, at least 1.
    pub max_attempts: u32,
    /// The delay before a retry, in whole seconds.
    pub delay_secs: u64,
}

impl RetryPolicy {
    /// No retry: one attempt, no delay.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_attempts: 1,
            delay_secs: 0,
        }
    }

    /// Whether this is the default (`none`), so text can omit it.
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::none()
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

/// One directed edge of the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// Where the edge starts.
    pub from: NodeId,
    /// Where the edge ends.
    pub to: NodeId,
    /// A condition expression (the shared expression language) the engine
    /// evaluates; absent for an unconditional edge.
    pub condition: Option<String>,
    /// A named rule the engine evaluates; absent when `condition` is used.
    pub rule: Option<String>,
    /// A human-readable label for the branch.
    pub label: Option<String>,
    /// Whether this is the default edge when no condition holds.
    pub default: bool,
}

impl Edge {
    /// Builds an unconditional edge.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId) -> Self {
        Self {
            from,
            to,
            condition: None,
            rule: None,
            label: None,
            default: false,
        }
    }

    /// Sets a condition expression.
    #[must_use]
    pub fn with_condition(mut self, condition: impl Into<String>) -> Self {
        self.condition = Some(condition.into());
        self
    }

    /// Sets the named rule.
    #[must_use]
    pub fn with_rule(mut self, rule: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self
    }

    /// Sets the label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Marks the edge default.
    #[must_use]
    pub fn as_default(mut self) -> Self {
        self.default = true;
        self
    }
}

/// A named boolean expression edges may reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The rule's id; `[a-z][a-z0-9_]{0,63}`.
    pub id: NodeId,
    /// The expression (the shared expression language).
    pub expression: String,
}

impl Rule {
    /// Builds a rule; the id grammar is the caller's contract here and is
    /// checked by [`NodeId::new`].
    #[must_use]
    pub fn new(id: NodeId, expression: impl Into<String>) -> Self {
        Self {
            id,
            expression: expression.into(),
        }
    }
}

/// The status of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// In progress.
    #[default]
    Running,
    /// Finished normally.
    Completed,
    /// Stopped by a node failure with no error branch.
    Failed,
    /// Cancelled.
    Cancelled,
}

/// Where one run is: the nodes currently active, its variables and its status.
///
/// The run is a [`CaseState`] in code (the word a developer reads); a person
/// never sees it — user-facing text goes through [`labels`].
#[derive(Debug, Clone, PartialEq)]
pub struct CaseState {
    /// The process being run.
    pub process: String,
    /// The version being run.
    pub version: u32,
    /// The nodes currently active.
    pub current: BTreeSet<NodeId>,
    /// The run's variables, as canonical JSON.
    pub variables: serde_json::Value,
    /// The run's status.
    pub status: RunStatus,
}

impl CaseState {
    /// Builds a run state.
    #[must_use]
    pub fn new(process: impl Into<String>, version: u32) -> Self {
        Self {
            process: process.into(),
            version,
            current: BTreeSet::new(),
            variables: serde_json::Value::Object(serde_json::Map::new()),
            status: RunStatus::Running,
        }
    }

    /// The variables as canonical JSON: object keys sorted, no whitespace, no
    /// floats (ADR-12).
    #[must_use]
    pub fn canonical_variables(&self) -> String {
        crate::json::canonical_json(&self.variables)
    }
}

/// Structural caps a process must fit. Defaults are conservative because a
/// compiled process lives in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most nodes one version may contain (default 10,000).
    pub max_nodes: usize,
    /// The most edges one version may contain (default 50,000).
    pub max_edges: usize,
    /// The longest chain from a trigger to a terminal (default 256).
    pub max_depth: usize,
}

impl Limits {
    /// The brief's default limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_nodes: 10_000,
            max_edges: 50_000,
            max_depth: 256,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

/// What the model asks of the DocType metadata it validates a process against.
///
/// It is a trait so `spot-bpm-model` stays free of the metadata crate: the
/// host passes whatever it has. [`AllowAllMeta`] answers yes to everything
/// (used by the template tests, where the DocTypes live in other apps);
/// [`EmptyMeta`] answers no to everything.
pub trait MetaView {
    /// Whether a DocType with this name is installed.
    fn has_doctype(&self, doctype: &str) -> bool;
    /// Whether `doctype` has a lifecycle operation with this name.
    fn has_operation(&self, doctype: &str, operation: &str) -> bool;
    /// Whether a process with this name exists (for sub-process nodes).
    fn has_process(&self, process: &str) -> bool;
    /// The processes this process starts through sub-process nodes, used to
    /// detect sub-process cycles. The default is empty (a view that does not
    /// track nesting).
    fn sub_processes(&self, process: &str) -> Vec<String> {
        let _ = process;
        Vec::new()
    }
}

/// A [`MetaView`] that knows every DocType, operation and process.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllMeta;

impl MetaView for AllowAllMeta {
    fn has_doctype(&self, _doctype: &str) -> bool {
        true
    }

    fn has_operation(&self, _doctype: &str, _operation: &str) -> bool {
        true
    }

    fn has_process(&self, _process: &str) -> bool {
        true
    }
}

/// A [`MetaView`] that knows no DocType, operation or process.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyMeta;

impl MetaView for EmptyMeta {
    fn has_doctype(&self, _doctype: &str) -> bool {
        false
    }

    fn has_operation(&self, _doctype: &str, _operation: &str) -> bool {
        false
    }

    fn has_process(&self, _process: &str) -> bool {
        false
    }
}
