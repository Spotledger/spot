//! The deployment model's value types: edition, platform, topology, backend, coordination,
//! sync, apps, overrides, migrations, connectivity and site policies.
//!
//! Every type validates on construction and serialises with stable names, so the model's
//! canonical form (and therefore its digest) does not depend on map iteration order.
//!

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use spot_self_model::ChangeKind;

use crate::DeploymentError;
use crate::error::{MAX_TEXT_BYTES, check_text};

/// The wire format of the canonical model and the domain tag of its digest.
///
/// Spelled in pieces because a dotted lower-case literal in a source file is the error-code
/// catalogue's vocabulary, and a format tag is not an error code.
pub const FORMAT: &str = concat!("spot", ".", "deployment", ".", "model", ".", "v1");

/// How many migration runs the model keeps, newest last.
pub const MIGRATION_HISTORY_LIMIT: usize = 20;

/// How many epoch ledger entries the model keeps, newest last.
pub const EPOCH_LEDGER_LIMIT: usize = 64;

/// How many recently applied event ids the model remembers for idempotency.
///
/// A redelivered event older than this window is still refused as [`crate::ApplyOutcome::Stale`],
/// so the window only bounds memory; it never weakens duplicate detection.
pub const SEEN_EVENT_LIMIT: u64 = 256;

/// Separates the digest domain from the canonical bytes.
const DIGEST_DOMAIN: &[u8] = b"spot.deployment.model.v1\0";

/// A SHA-256 commitment to the model's facts, as 64 lower-case hex characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// A digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DIGEST_DOMAIN);
        hasher.update(bytes);
        let mut hex = String::with_capacity(64);
        for byte in hasher.finalize() {
            use fmt::Write as _;
            // Writing into a String cannot fail; the result is discarded rather than unwrapped.
            let _ = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// The hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The licensed edition, as raw lower-case text (`community`, `business`, `enterprise`).
///
/// The edition is data: this crate does not decide which editions exist. It only refuses text
/// that could not be an edition name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Edition(String);

impl Edition {
    /// Validates an edition name.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the text is empty, longer than 64 bytes, does not
    /// start with a lower-case ASCII letter, or holds a character outside `a-z`, `0-9`, `.`,
    /// `_` and `-`.
    pub fn new(text: impl Into<String>) -> Result<Self, DeploymentError> {
        let text = text.into();
        check_text("edition", &text, 64)?;
        let mut chars = text.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let rest_ok = chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
        if !first_ok || !rest_ok {
            return Err(DeploymentError::invalid(
                "edition",
                "an edition is lower-case letters, digits, `.`, `_` and `-`, starting with a letter",
            ));
        }
        Ok(Self(text))
    }

    /// The edition's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The operating-system family the installation runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    /// Linux.
    Linux,
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
}

impl OsFamily {
    /// Every family.
    pub const ALL: [OsFamily; 3] = [Self::Linux, Self::Windows, Self::MacOs];

    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::MacOs => "macos",
        }
    }

    /// The family a text form names.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] for any other text.
    pub fn parse(text: &str) -> Result<Self, DeploymentError> {
        match text {
            "linux" => Ok(Self::Linux),
            "windows" => Ok(Self::Windows),
            "macos" => Ok(Self::MacOs),
            other => Err(DeploymentError::invalid(
                "platform.os",
                format!("`{other}` is not an OS family"),
            )),
        }
    }
}

impl fmt::Display for OsFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The platform the runtime runs on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    /// The operating-system family.
    pub os: OsFamily,
    /// Its version or build text, as the host reports it.
    pub version: String,
}

impl Platform {
    /// A platform.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the version is empty or too long.
    pub fn new(os: OsFamily, version: impl Into<String>) -> Result<Self, DeploymentError> {
        let version = version.into();
        check_text("platform.version", &version, 128)?;
        Ok(Self { os, version })
    }
}

/// One role a node serves, using the runtime's wire names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRole {
    /// Serves the web UI and API.
    Web,
    /// Runs background jobs.
    Jobs,
    /// Serves realtime subscriptions.
    Realtime,
    /// Owns the PostgreSQL data.
    Database,
    /// Serves a branch edge with offline capability.
    BranchEdge,
}

impl NodeRole {
    /// Every role, in the order the runtime lists them.
    pub const ALL: [NodeRole; 5] = [
        Self::Web,
        Self::Jobs,
        Self::Realtime,
        Self::Database,
        Self::BranchEdge,
    ];

    /// The stable wire name, the same one `spot-agent-runtime` uses in the role setting.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Jobs => "jobs",
            Self::Realtime => "realtime",
            Self::Database => "database",
            Self::BranchEdge => "branch-edge",
        }
    }

    /// The role a wire name names, when it is one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == text)
    }
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One node of the topology and the roles it serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentNode {
    /// The roles, in role order.
    pub roles: BTreeSet<NodeRole>,
}

impl DeploymentNode {
    /// A node with at least one role.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the role set is empty.
    pub fn new(roles: impl IntoIterator<Item = NodeRole>) -> Result<Self, DeploymentError> {
        let roles: BTreeSet<NodeRole> = roles.into_iter().collect();
        if roles.is_empty() {
            return Err(DeploymentError::invalid(
                "topology.node.roles",
                "a node serves at least one role",
            ));
        }
        Ok(Self { roles })
    }
}

/// Whether one node does everything or several nodes share the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyMode {
    /// One node runs every role.
    Standalone,
    /// Several nodes share the roles.
    Cluster,
}

impl TopologyMode {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Cluster => "cluster",
        }
    }
}

/// Where this installation sits: a head office or a branch edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// The head office, the canonical data owner.
    HeadOffice,
    /// A branch edge, which must keep working offline.
    BranchEdge,
}

impl Placement {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HeadOffice => "head-office",
            Self::BranchEdge => "branch-edge",
        }
    }
}

/// The deployment's topology: its shape, its placement and its nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    /// Standalone or cluster.
    pub mode: TopologyMode,
    /// Head office or branch edge.
    pub placement: Placement,
    /// The nodes, by name, in name order.
    pub nodes: BTreeMap<String, DeploymentNode>,
}

impl Topology {
    /// A topology with at least one node.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when a node name is empty or too long, or the topology
    /// holds no node.
    pub fn new(
        mode: TopologyMode,
        placement: Placement,
        nodes: BTreeMap<String, DeploymentNode>,
    ) -> Result<Self, DeploymentError> {
        if nodes.is_empty() {
            return Err(DeploymentError::invalid(
                "topology.nodes",
                "a deployment has at least one node",
            ));
        }
        for name in nodes.keys() {
            check_text("topology.node", name, 128)?;
        }
        Ok(Self {
            mode,
            placement,
            nodes,
        })
    }

    /// One node's entry.
    #[must_use]
    pub fn node(&self, name: &str) -> Option<&DeploymentNode> {
        self.nodes.get(name)
    }

    /// Whether this is one node running every role.
    #[must_use]
    pub fn is_standalone(&self) -> bool {
        self.mode == TopologyMode::Standalone
    }
}

/// The backend engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendEngine {
    /// PostgreSQL, the only backend in v1.
    Postgres,
}

impl BackendEngine {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
        }
    }
}

/// Whether a replica accepts writes or streams from a primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaRole {
    /// Accepts writes.
    Primary,
    /// Streams from a primary.
    Standby,
}

impl ReplicaRole {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Standby => "standby",
        }
    }
}

/// One database replica.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replica {
    /// The node that runs it.
    pub node: String,
    /// Primary or standby.
    pub role: ReplicaRole,
    /// Whether a standby is streaming right now.
    pub streaming: bool,
}

impl Replica {
    /// A replica.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the node name is empty or too long.
    pub fn new(
        node: impl Into<String>,
        role: ReplicaRole,
        streaming: bool,
    ) -> Result<Self, DeploymentError> {
        let node = node.into();
        check_text("backend.replica.node", &node, 128)?;
        Ok(Self {
            node,
            role,
            streaming,
        })
    }
}

/// The database backend: engine, version and replicas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backend {
    /// The engine.
    pub engine: BackendEngine,
    /// The server version, as PostgreSQL reports it.
    pub version: String,
    /// The replicas, primary first, then standbys by node name.
    pub replicas: Vec<Replica>,
}

impl Backend {
    /// A backend.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the version is empty or too long, or two replicas
    /// name the same node.
    pub fn new(
        engine: BackendEngine,
        version: impl Into<String>,
        replicas: Vec<Replica>,
    ) -> Result<Self, DeploymentError> {
        let version = version.into();
        check_text("backend.version", &version, 128)?;
        let mut seen = BTreeSet::new();
        for replica in &replicas {
            if !seen.insert(replica.node.clone()) {
                return Err(DeploymentError::invalid(
                    "backend.replicas",
                    format!("node {} appears twice", replica.node),
                ));
            }
        }
        Ok(Self {
            engine,
            version,
            replicas,
        })
    }

    /// The primary replica, when the model knows one.
    #[must_use]
    pub fn primary(&self) -> Option<&Replica> {
        self.replicas
            .iter()
            .find(|replica| replica.role == ReplicaRole::Primary)
    }

    /// How many standbys the backend has.
    #[must_use]
    pub fn standby_count(&self) -> usize {
        self.replicas
            .iter()
            .filter(|replica| replica.role == ReplicaRole::Standby)
            .count()
    }
}

/// The cache backend the deployment uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBackend {
    /// The platform's own PostgreSQL-backed cache.
    Database,
    /// Redis.
    Redis,
}

/// What coordinates nodes and invalidations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coordinator {
    /// The in-process coordinator (a standalone or a single web node).
    InProcess,
    /// Redis.
    Redis,
}

/// The cache and coordination profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coordination {
    /// The cache backend.
    pub cache: CacheBackend,
    /// The cross-node coordinator.
    pub coordinator: Coordinator,
}

impl Coordination {
    /// A profile.
    #[must_use]
    pub fn new(cache: CacheBackend, coordinator: Coordinator) -> Self {
        Self { cache, coordinator }
    }
}

/// How this installation takes part in synchronisation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// No synchronisation; one site owns its data.
    Standalone,
    /// The hub other nodes synchronise with.
    Hub,
    /// An edge that synchronises with a hub.
    Edge,
}

impl SyncMode {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Hub => "hub",
            Self::Edge => "edge",
        }
    }
}

/// The sync profile: mode, the hub it talks to, and the policy revision in force.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProfile {
    /// The mode.
    pub mode: SyncMode,
    /// The hub's name, required for hub and edge modes.
    pub hub: Option<String>,
    /// The sync policy revision the site resolved, when one is recorded.
    pub policy_revision: Option<String>,
}

impl SyncProfile {
    /// A sync profile.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when a hub name is missing for a synchronising mode, or a
    /// provided text is empty or too long.
    pub fn new(
        mode: SyncMode,
        hub: Option<String>,
        policy_revision: Option<String>,
    ) -> Result<Self, DeploymentError> {
        if mode != SyncMode::Standalone && hub.is_none() {
            return Err(DeploymentError::invalid(
                "sync.hub",
                "a synchronising installation names its hub",
            ));
        }
        if let Some(hub) = &hub {
            check_text("sync.hub", hub, MAX_TEXT_BYTES)?;
        }
        if let Some(revision) = &policy_revision {
            check_text("sync.policy_revision", revision, MAX_TEXT_BYTES)?;
        }
        Ok(Self {
            mode,
            hub,
            policy_revision,
        })
    }

    /// A standalone profile with no hub.
    #[must_use]
    pub fn standalone() -> Self {
        Self {
            mode: SyncMode::Standalone,
            hub: None,
            policy_revision: None,
        }
    }
}

/// Whether an installed app is active or switched off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    /// Installed and active.
    Installed,
    /// Installed but disabled for this site.
    Disabled,
}

impl AppState {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Disabled => "disabled",
        }
    }
}

/// One installed app: its version and state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEntry {
    /// The installed version, as the installer recorded it.
    pub version: String,
    /// Whether it is active.
    pub state: AppState,
}

impl AppEntry {
    /// An app entry.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the version is empty or too long.
    pub fn new(version: impl Into<String>, state: AppState) -> Result<Self, DeploymentError> {
        let version = version.into();
        check_text("apps version", &version, 128)?;
        Ok(Self { version, state })
    }
}

/// How an override changes the step it targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideKind {
    /// The replacement runs before the target.
    Before,
    /// The replacement runs after the target.
    After,
    /// The replacement wraps the target.
    Wrap,
    /// The replacement runs in the target's place.
    Replace,
    /// The target does not run.
    Disable,
}

impl OverrideKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
            Self::Wrap => "wrap",
            Self::Replace => "replace",
            Self::Disable => "disable",
        }
    }
}

/// One seam override the site has applied.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Override {
    /// The app that declared it.
    pub app: String,
    /// The step it changes.
    pub target: String,
    /// How it changes it.
    pub kind: OverrideKind,
    /// The step that runs instead of or around the target; absent for a disable.
    pub by: Option<String>,
}

impl Override {
    /// An override.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the app or target is empty or too long, a non-disable
    /// override names no replacement, or a disable names one.
    pub fn new(
        app: impl Into<String>,
        target: impl Into<String>,
        kind: OverrideKind,
        by: Option<String>,
    ) -> Result<Self, DeploymentError> {
        let app = app.into();
        let target = target.into();
        check_text("overrides.app", &app, MAX_TEXT_BYTES)?;
        check_text("overrides.target", &target, MAX_TEXT_BYTES)?;
        match (kind, &by) {
            (OverrideKind::Disable, Some(_)) => {
                return Err(DeploymentError::invalid(
                    "overrides.by",
                    "a disable override runs no replacement",
                ));
            }
            (OverrideKind::Disable, None) => {}
            (_, None) => {
                return Err(DeploymentError::invalid(
                    "overrides.by",
                    "an override names the step that runs instead",
                ));
            }
            (_, Some(by)) => check_text("overrides.by", by, MAX_TEXT_BYTES)?,
        }
        Ok(Self {
            app,
            target,
            kind,
            by,
        })
    }
}

/// One migration run in the model's history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MigrationRun {
    /// The run id.
    pub run: String,
    /// Its outcome, in the audit's words (`applied`, `rolled_back`, ...).
    pub outcome: String,
    /// When it finished, in unix microseconds supplied by the caller.
    pub applied_at: i64,
}

impl MigrationRun {
    /// A migration run.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the run or outcome is empty or too long.
    pub fn new(
        run: impl Into<String>,
        outcome: impl Into<String>,
        applied_at: i64,
    ) -> Result<Self, DeploymentError> {
        let run = run.into();
        let outcome = outcome.into();
        check_text("migrations.run", &run, MAX_TEXT_BYTES)?;
        check_text("migrations.outcome", &outcome, MAX_TEXT_BYTES)?;
        Ok(Self {
            run,
            outcome,
            applied_at,
        })
    }
}

/// Whether this installation can reach its hub, and when it last synchronised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connectivity {
    /// Whether the hub answered last time it was asked.
    pub hub_reachable: bool,
    /// The last successful sync, in unix microseconds, when one is known.
    pub last_sync_at: Option<i64>,
}

impl Connectivity {
    /// A connectivity reading.
    #[must_use]
    pub fn new(hub_reachable: bool, last_sync_at: Option<i64>) -> Self {
        Self {
            hub_reachable,
            last_sync_at,
        }
    }
}

/// The per-site replay policy for migrations and rehearsals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    /// Never replay.
    Off,
    /// Replay when the plan declares a behaviour change or a sensitive entry.
    Auto,
    /// Always replay.
    Always,
}

impl ReplayPolicy {
    /// The stable text form, as the site setting spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Always => "always",
        }
    }
}

/// The per-site approval policy, summarised by the numbers an agent needs before acting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    /// Whether an additive migration needs no approval.
    pub additive_auto_approve: bool,
    /// Distinct approvers a needs-review migration requires.
    pub review_approvers: u32,
    /// Distinct approvers a destructive migration requires.
    pub destructive_approvers: u32,
}

impl ApprovalPolicy {
    /// Validates the approver counts: a review needs one, a destructive change two.
    pub fn new(
        additive_auto_approve: bool,
        review_approvers: u32,
        destructive_approvers: u32,
    ) -> Result<Self, DeploymentError> {
        if review_approvers == 0 {
            return Err(DeploymentError::invalid(
                "policies.approval.review_approvers",
                "a review needs at least one approver",
            ));
        }
        if destructive_approvers < 2 {
            return Err(DeploymentError::invalid(
                "policies.approval.destructive_approvers",
                "a destructive change needs two approvers",
            ));
        }
        Ok(Self {
            additive_auto_approve,
            review_approvers,
            destructive_approvers,
        })
    }
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            additive_auto_approve: true,
            review_approvers: 1,
            destructive_approvers: 2,
        }
    }
}

/// The site policies an agent must respect before it acts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePolicies {
    /// The replay policy.
    pub replay: ReplayPolicy,
    /// The approval policy.
    pub approval: ApprovalPolicy,
}

impl SitePolicies {
    /// The policies.
    #[must_use]
    pub fn new(replay: ReplayPolicy, approval: ApprovalPolicy) -> Self {
        Self { replay, approval }
    }
}

impl Default for SitePolicies {
    fn default() -> Self {
        Self {
            replay: ReplayPolicy::Off,
            approval: ApprovalPolicy::default(),
        }
    }
}

/// Which authoritative source a fact came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSourceKind {
    /// The installer's installed-app state.
    Installer,
    /// Resolved runtime settings.
    Settings,
    /// The host agent's runtime profile, roles and replica status.
    Runtime,
    /// Fleet health observations.
    FleetHealth,
    /// The step and seam registry's overrides.
    Seams,
    /// The agent registry's identity and tier.
    Agent,
    /// An in-memory fact set (tests and simple hosts).
    Memory,
}

impl FactSourceKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installer => "installer",
            Self::Settings => "settings",
            Self::Runtime => "runtime",
            Self::FleetHealth => "fleet_health",
            Self::Seams => "seams",
            Self::Agent => "agent",
            Self::Memory => "memory",
        }
    }
}

impl fmt::Display for FactSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line of the epoch ledger: what moved the model to `epoch`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EpochEntry {
    /// The epoch this change produced.
    pub epoch: u64,
    /// The event id, or `refresh` for a full refresh.
    pub event: String,
    /// The thing the change is about.
    pub subject: String,
    /// What kind of change it was.
    pub change: ChangeKind,
    /// Unix microseconds supplied by the caller.
    pub at: i64,
    /// The record that proves it, when the source or event named one.
    pub evidence: Option<spot_self_model::EvidenceRef>,
}
