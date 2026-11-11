//! What the wizard is asked to deploy: hosts, the roles they serve and the runtime profile,
//! validated as a whole so the operator sees every problem at once.
//!
//! The vocabulary is the agent runtime's own: [`Role`] names exactly the roles
//! [`spot_agent_runtime::NodeRole`] serves and [`Profile`] exactly the profiles
//! [`spot_agent_runtime::Profile`] applies. [`Topology::validate`] is therefore the
//! pre-check of a write the runtime would otherwise refuse later, with the difference that
//! a wizard reports **every** broken rule in one pass, each with the fix in plain words.
//!
//! # Rules (WP-ADM-13.T1)
//!
//! - The topology names at least one node, and each host once.
//! - At least one node serves `database`, and at least one serves `web`, `jobs` and
//!   `realtime` each.
//! - Web nodes are reached through a stated address ([`Topology::web_address`]); an address
//!   without a web node is a mistake too.
//! - Exactly one database node is the primary when the topology has more than one database
//!   node; a primary must be a database node. With a single database node it is the primary
//!   without being marked.
//! - Every other database node is a replica and needs the primary to clone from; a
//!   database node is never a branch edge.
//! - A `branch-edge` node serves that role alone: it keeps working offline, so it cannot
//!   also own data or accept web traffic.
//! - `standalone` is exactly one node serving web, jobs, realtime and database;
//!   `cluster-basic` and `cluster-redis` are at least two nodes.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::WizardError;

/// One node role, spelled as the agent runtime spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Serves the web UI and the API.
    Web,
    /// Runs background jobs.
    Jobs,
    /// Serves realtime subscriptions and WebSocket connections.
    Realtime,
    /// Owns the PostgreSQL data.
    Database,
    /// Serves a branch edge that keeps working offline.
    BranchEdge,
}

impl Role {
    /// Every role, in the order the console lists them.
    pub const ALL: [Role; 5] = [
        Role::Web,
        Role::Jobs,
        Role::Realtime,
        Role::Database,
        Role::BranchEdge,
    ];

    /// The lower-case wire name, identical to [`spot_agent_runtime::NodeRole::name`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Jobs => "jobs",
            Self::Realtime => "realtime",
            Self::Database => "database",
            Self::BranchEdge => "branch-edge",
        }
    }

    /// The role named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == name)
    }

    /// The agent runtime role this one writes through `node.role.set`.
    #[must_use]
    pub fn to_runtime(self) -> spot_agent_runtime::NodeRole {
        match self {
            Self::Web => spot_agent_runtime::NodeRole::Web,
            Self::Jobs => spot_agent_runtime::NodeRole::Jobs,
            Self::Realtime => spot_agent_runtime::NodeRole::Realtime,
            Self::Database => spot_agent_runtime::NodeRole::Database,
            Self::BranchEdge => spot_agent_runtime::NodeRole::BranchEdge,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The runtime profile a deployment applies, spelled as the agent runtime spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// One node does everything.
    Standalone,
    /// Separate web, jobs and realtime nodes with the database-backed cache.
    ClusterBasic,
    /// Separate web, jobs and realtime nodes with Redis for cache and coordination.
    ClusterRedis,
}

impl Profile {
    /// Every profile, in the order the console lists them.
    pub const ALL: [Profile; 3] = [
        Profile::Standalone,
        Profile::ClusterBasic,
        Profile::ClusterRedis,
    ];

    /// The lower-case wire name, identical to [`spot_agent_runtime::Profile::name`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::ClusterBasic => "cluster-basic",
            Self::ClusterRedis => "cluster-redis",
        }
    }

    /// The profile named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|profile| profile.name() == name)
    }

    /// Whether the profile is a cluster of separate nodes.
    #[must_use]
    pub fn is_cluster(self) -> bool {
        !matches!(self, Self::Standalone)
    }

    /// The agent runtime profile this one applies through `runtime.profile.apply`.
    #[must_use]
    pub fn to_runtime(self) -> spot_agent_runtime::Profile {
        match self {
            Self::Standalone => spot_agent_runtime::Profile::Standalone,
            Self::ClusterBasic => spot_agent_runtime::Profile::ClusterBasic,
            Self::ClusterRedis => spot_agent_runtime::Profile::ClusterRedis,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A host the wizard deploys to: a DNS name or IP address, lower-case, with no port and no
/// scheme (the port belongs to a role's own setting).
///
/// Construction validates the shape, so a [`Host`] that exists is safe to put in a signed
/// command's parameter. Serde deserializes through [`Host::try_new`], so a malformed host
/// cannot enter a topology from a payload either.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Host(String);

/// Why a host was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The host is empty.
    #[error("a host name cannot be empty")]
    Empty,
    /// The host is longer than 255 characters.
    #[error("a host name is at most 255 characters, got {len}")]
    TooLong {
        /// How many characters the text had.
        len: usize,
    },
    /// The host contains a character a host name cannot carry.
    #[error("a host name may only contain letters, digits, dots and hyphens, got `{text}`")]
    Character {
        /// The offending text.
        text: String,
    },
    /// The host starts or ends with a dot or a hyphen.
    #[error("a host name cannot start or end with a dot or a hyphen, got `{text}`")]
    Edge {
        /// The offending text.
        text: String,
    },
}

impl Host {
    /// The host named `text`, lower-cased, when it is a plain DNS name or IP address.
    ///
    /// # Errors
    /// [`HostError`] when the text is empty, longer than 255 characters, carries anything
    /// but ASCII letters, digits, dots and hyphens, or starts or ends with a dot or hyphen.
    pub fn try_new(text: &str) -> Result<Self, HostError> {
        if text.is_empty() {
            return Err(HostError::Empty);
        }
        if text.chars().count() > 255 {
            return Err(HostError::TooLong {
                len: text.chars().count(),
            });
        }
        if !text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(HostError::Character {
                text: text.to_owned(),
            });
        }
        if text.starts_with(['.', '-']) || text.ends_with(['.', '-']) {
            return Err(HostError::Edge {
                text: text.to_owned(),
            });
        }
        Ok(Self(text.to_lowercase()))
    }

    /// The host as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Host {
    type Error = HostError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::try_new(&text)
    }
}

impl From<Host> for String {
    fn from(host: Host) -> Self {
        host.0
    }
}

/// One server the operator wants to deploy to: its host and the roles it serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// The server's host.
    pub host: Host,
    /// The roles it serves, in role order, without duplicates.
    pub roles: Vec<Role>,
    /// Whether it is the database primary. At most one node in a topology may be.
    #[serde(default)]
    pub primary: bool,
}

impl NodeSpec {
    /// A node with `host` and `roles`, not marked primary.
    ///
    /// The roles are sorted and de-duplicated; whether the combination is legal is
    /// [`Topology::validate`]'s job, so the wizard can report every problem together.
    ///
    /// # Errors
    /// [`HostError`] when `host` is not a valid host name.
    pub fn new(host: &str, roles: &[Role]) -> Result<Self, HostError> {
        let mut roles = roles.to_vec();
        roles.sort_unstable();
        roles.dedup();
        Ok(Self {
            host: Host::try_new(host)?,
            roles,
            primary: false,
        })
    }

    /// The same node, marked as the database primary.
    #[must_use]
    pub fn as_primary(mut self) -> Self {
        self.primary = true;
        self
    }

    /// Whether the node serves `role`.
    #[must_use]
    pub fn serves(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    /// Whether the node's only role is a branch edge.
    #[must_use]
    pub fn is_branch_edge(&self) -> bool {
        self.roles == [Role::BranchEdge]
    }

    /// Whether the node owns database data (primary or replica).
    #[must_use]
    pub fn is_database(&self) -> bool {
        self.serves(Role::Database)
    }
}

/// What the wizard is asked to deploy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    /// The servers, in the order the operator entered them.
    pub nodes: Vec<NodeSpec>,
    /// The runtime profile to apply on every node.
    pub profile: Profile,
    /// The address users reach the web nodes through, for example `erp.example.com`;
    /// required as soon as one node serves `web`.
    #[serde(default)]
    pub web_address: Option<String>,
}

impl Topology {
    /// A topology of `nodes` under `profile`, with no web address stated yet.
    #[must_use]
    pub fn new(nodes: Vec<NodeSpec>, profile: Profile) -> Self {
        Self {
            nodes,
            profile,
            web_address: None,
        }
    }

    /// The same topology, stating the address users reach the web nodes through.
    #[must_use]
    pub fn with_web_address(mut self, address: &str) -> Self {
        self.web_address = Some(address.to_owned());
        self
    }

    /// The node serving `role`, if any.
    #[must_use]
    pub fn node_serving(&self, role: Role) -> Option<&NodeSpec> {
        self.nodes.iter().find(|node| node.serves(role))
    }

    /// Every node serving `role`, in topology order.
    pub fn nodes_serving(&self, role: Role) -> impl Iterator<Item = &NodeSpec> {
        self.nodes.iter().filter(move |node| node.serves(role))
    }

    /// The database primary: the marked node, or — when none or several are marked, which
    /// validation refuses — the first database node in host order.
    #[must_use]
    pub fn primary(&self) -> Option<&NodeSpec> {
        let marked: Vec<&NodeSpec> = self.nodes.iter().filter(|node| node.primary).collect();
        match marked.as_slice() {
            [only] => Some(only),
            _ => self
                .nodes
                .iter()
                .filter(|node| node.is_database())
                .min_by(|left, right| left.host.cmp(&right.host)),
        }
    }

    /// The database replicas: every database node that is not the primary.
    #[must_use]
    pub fn replicas(&self) -> Vec<&NodeSpec> {
        let primary = self.primary().map(|node| node.host.clone());
        self.nodes
            .iter()
            .filter(|node| node.is_database() && Some(node.host.clone()) != primary)
            .collect()
    }

    /// Every host in topology order.
    #[must_use]
    pub fn hosts(&self) -> Vec<&Host> {
        self.nodes.iter().map(|node| &node.host).collect()
    }

    /// Every rule the topology breaks, in a stable order: the console shows the whole list
    /// at once, each with its fix.
    #[must_use]
    pub fn validate(&self) -> Vec<Violation> {
        let mut violations = Vec::new();
        if self.nodes.is_empty() {
            violations.push(Violation::new(
                Rule::EmptyTopology,
                Vec::new(),
                "the deployment has no servers",
                "Add at least one server, and give it the database role.",
            ));
            return violations;
        }

        let mut seen = BTreeSet::new();
        for node in &self.nodes {
            if !seen.insert(node.host.clone()) {
                violations.push(Violation::new(
                    Rule::DuplicateHost,
                    vec![node.host.clone()],
                    format!("{} is listed more than once", node.host),
                    "Keep one entry per server and put every role it serves on that entry.",
                ));
            }
        }

        self.check_roles(&mut violations);
        self.check_web_address(&mut violations);
        self.check_primary(&mut violations);
        self.check_branch_edges(&mut violations);
        self.check_profile(&mut violations);
        violations
    }

    fn check_roles(&self, violations: &mut Vec<Violation>) {
        if self.node_serving(Role::Database).is_none() {
            violations.push(Violation::new(
                Rule::NoDatabase,
                Vec::new(),
                "no server owns the database",
                "Give one server the database role: every deployment needs one canonical data owner.",
            ));
        }
        for (role, fix) in [
            (
                Role::Web,
                "Give at least one server the web role so users can reach the deployment.",
            ),
            (
                Role::Jobs,
                "Give at least one server the jobs role; background work has nowhere to run otherwise.",
            ),
            (
                Role::Realtime,
                "Give at least one server the realtime role; live updates need a realtime node.",
            ),
        ] {
            if self.node_serving(role).is_none() {
                let rule = match role {
                    Role::Web => Rule::NoWeb,
                    Role::Jobs => Rule::NoJobs,
                    _ => Rule::NoRealtime,
                };
                violations.push(Violation::new(
                    rule,
                    Vec::new(),
                    format!("no server serves the {role} role"),
                    fix,
                ));
            }
        }
        for node in &self.nodes {
            if node.roles.is_empty() {
                violations.push(Violation::new(
                    Rule::NoRoles,
                    vec![node.host.clone()],
                    format!("{} has no roles", node.host),
                    "Choose at least one role for the server, or remove it from the deployment.",
                ));
            }
        }
    }

    fn check_web_address(&self, violations: &mut Vec<Violation>) {
        let has_web = self.node_serving(Role::Web).is_some();
        match (has_web, self.web_address.as_deref()) {
            (true, None) => violations.push(Violation::new(
                Rule::WebAddressMissing,
                Vec::new(),
                "the web nodes have no address users can reach",
                "State the address the web nodes answer on, for example erp.example.com.",
            )),
            (true, Some(address)) => {
                if let Some(problem) = web_address_problem(address) {
                    violations.push(Violation::new(
                        Rule::WebAddressInvalid,
                        Vec::new(),
                        format!("the web address `{address}` is not usable: {problem}"),
                        "State a plain host name or host name and port, with no scheme or path.",
                    ));
                }
            }
            (false, Some(_)) => violations.push(Violation::new(
                Rule::WebAddressWithoutWeb,
                Vec::new(),
                "a web address is stated but no server serves the web role",
                "Give a server the web role, or clear the web address.",
            )),
            (false, None) => {}
        }
    }

    fn check_primary(&self, violations: &mut Vec<Violation>) {
        let marked: Vec<&NodeSpec> = self.nodes.iter().filter(|node| node.primary).collect();
        if marked.len() > 1 {
            let hosts: Vec<Host> = marked.iter().map(|node| node.host.clone()).collect();
            violations.push(Violation::new(
                Rule::MultiplePrimary,
                hosts,
                format!("{} servers are marked primary", marked.len()),
                "Mark exactly one database server as the primary and leave the others as replicas.",
            ));
        }
        for node in &marked {
            if !node.is_database() {
                violations.push(Violation::new(
                    Rule::PrimaryNotDatabase,
                    vec![node.host.clone()],
                    format!(
                        "{} is marked primary but does not serve the database role",
                        node.host
                    ),
                    "Give the server the database role, or clear its primary mark.",
                ));
            }
        }
    }

    fn check_branch_edges(&self, violations: &mut Vec<Violation>) {
        for node in &self.nodes {
            if node.serves(Role::BranchEdge) && !node.is_branch_edge() {
                violations.push(Violation::new(
                    Rule::BranchEdgeAlone,
                    vec![node.host.clone()],
                    format!(
                        "{} serves the branch-edge role together with {}",
                        node.host,
                        node.roles
                            .iter()
                            .filter(|role| **role != Role::BranchEdge)
                            .map(|role| role.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    "Give the branch edge its own server, or remove the branch-edge role from this one.",
                ));
            }
            if node.primary && node.serves(Role::BranchEdge) {
                violations.push(Violation::new(
                    Rule::PrimaryBranchEdge,
                    vec![node.host.clone()],
                    format!("{} is a branch edge and the primary", node.host),
                    "A branch edge keeps working offline and cannot own the canonical data; move the primary to a database server.",
                ));
            }
        }
        if self.profile == Profile::Standalone && self.node_serving(Role::BranchEdge).is_some() {
            violations.push(Violation::new(
                Rule::StandaloneBranchEdge,
                Vec::new(),
                "a standalone deployment cannot also be a branch edge",
                "Use a cluster profile for branch edges, or remove the branch-edge role.",
            ));
        }
    }

    fn check_profile(&self, violations: &mut Vec<Violation>) {
        match self.profile {
            Profile::Standalone => {
                if self.nodes.len() != 1 {
                    violations.push(Violation::new(
                        Rule::StandaloneShape,
                        self.hosts().into_iter().cloned().collect(),
                        format!(
                            "a standalone deployment has one server, but {} are listed",
                            self.nodes.len()
                        ),
                        "Switch to cluster-basic (or cluster-redis) for several servers, or keep exactly one.",
                    ));
                }
                if let Some(node) = self.nodes.first()
                    && self.nodes.len() == 1
                {
                    for role in [Role::Web, Role::Jobs, Role::Realtime, Role::Database] {
                        if !node.serves(role) {
                            violations.push(Violation::new(
                                Rule::StandaloneShape,
                                vec![node.host.clone()],
                                format!(
                                    "the standalone server does not serve the {role} role"
                                ),
                                "A standalone node runs everything: give it web, jobs, realtime and database.",
                            ));
                        }
                    }
                }
            }
            Profile::ClusterBasic | Profile::ClusterRedis => {
                if self.nodes.len() < 2 {
                    violations.push(Violation::new(
                        Rule::ClusterTooSmall,
                        self.hosts().into_iter().cloned().collect(),
                        "a cluster needs at least two servers",
                        "Add a second server, or use the standalone profile for one server.",
                    ));
                }
            }
        }
    }

    /// Turns the broken rules into the error the planner returns, naming the first.
    ///
    /// # Errors
    /// [`WizardError::TopologyInvalid`] when [`Topology::validate`] finds anything.
    pub fn ensure_valid(&self) -> Result<(), WizardError> {
        let violations = self.validate();
        match violations.split_first() {
            None => Ok(()),
            Some((first, rest)) => Err(WizardError::TopologyInvalid {
                count: violations.len(),
                first: format!("{} ({} more)", first.describe(), rest.len()),
            }),
        }
    }
}

/// Whether a stated web address is a plain host name with an optional port.
fn web_address_problem(address: &str) -> Option<&'static str> {
    if address.is_empty() {
        return Some("it is empty");
    }
    if address.contains("://") {
        return Some("it carries a scheme");
    }
    if address.contains('/') {
        return Some("it carries a path");
    }
    if address.chars().any(char::is_whitespace) {
        return Some("it contains a space");
    }
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (address, None),
    };
    if Host::try_new(host).is_err() {
        return Some("the host part is not a valid host name");
    }
    if let Some(port) = port
        && port.parse::<u16>().map_or(true, |number| number == 0)
    {
        return Some("the port is not a number between 1 and 65535");
    }
    None
}

/// The rule a [`Violation`] breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// The topology lists no server.
    EmptyTopology,
    /// One host appears twice.
    DuplicateHost,
    /// A server has no roles.
    NoRoles,
    /// No server owns the database.
    NoDatabase,
    /// No server serves the web role.
    NoWeb,
    /// No server runs background jobs.
    NoJobs,
    /// No server serves realtime subscriptions.
    NoRealtime,
    /// Web nodes exist but no address is stated.
    WebAddressMissing,
    /// The stated address is not a plain host name and optional port.
    WebAddressInvalid,
    /// An address is stated without a web node.
    WebAddressWithoutWeb,
    /// More than one database server is marked primary.
    MultiplePrimary,
    /// A primary is marked but no database server owns the data.
    PrimaryNotDatabase,
    /// A branch edge shares its server with another role.
    BranchEdgeAlone,
    /// A branch edge is marked primary.
    PrimaryBranchEdge,
    /// A standalone deployment also names a branch edge.
    StandaloneBranchEdge,
    /// Standalone is not exactly one node serving every core role.
    StandaloneShape,
    /// A cluster profile has fewer than two servers.
    ClusterTooSmall,
}

impl Rule {
    /// The stable snake-case name the console logs and tests assert on.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::EmptyTopology => "empty_topology",
            Self::DuplicateHost => "duplicate_host",
            Self::NoRoles => "no_roles",
            Self::NoDatabase => "no_database",
            Self::NoWeb => "no_web",
            Self::NoJobs => "no_jobs",
            Self::NoRealtime => "no_realtime",
            Self::WebAddressMissing => "web_address_missing",
            Self::WebAddressInvalid => "web_address_invalid",
            Self::WebAddressWithoutWeb => "web_address_without_web",
            Self::MultiplePrimary => "multiple_primary",
            Self::PrimaryNotDatabase => "primary_not_database",
            Self::BranchEdgeAlone => "branch_edge_alone",
            Self::PrimaryBranchEdge => "primary_branch_edge",
            Self::StandaloneBranchEdge => "standalone_branch_edge",
            Self::StandaloneShape => "standalone_shape",
            Self::ClusterTooSmall => "cluster_too_small",
        }
    }
}

/// One broken deployment rule: what is wrong and how to fix it, in the operator's words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// The rule that broke.
    pub rule: Rule,
    /// The servers the violation is about, when it is about particular ones.
    pub hosts: Vec<Host>,
    /// What is wrong.
    pub problem: String,
    /// What the operator should do about it.
    pub fix: String,
}

impl Violation {
    /// A violation of `rule`, about `hosts`, with `problem` and `fix` in plain words.
    #[must_use]
    pub fn new(
        rule: Rule,
        hosts: Vec<Host>,
        problem: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            rule,
            hosts,
            problem: problem.into(),
            fix: fix.into(),
        }
    }

    /// The one sentence the console shows in the list.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{}: {}", self.problem, self.fix)
    }
}

#[cfg(test)]
mod tests;
