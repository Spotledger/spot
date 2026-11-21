//! The deployment plan: what the wizard runs, in which order, with what rollback.
//!
//! [`plan`] turns a validated [`Topology`] into [`DeployPlan`]: an ordered list of
//! [`Step`]s, each one a typed operation from the management catalogue, with explicit
//! dependencies. Independent steps carry no dependency edge between them, so the executor
//! (and the estimate) can run them in parallel.
//!
//! The plan is deterministic: the same topology always produces the same steps in the same
//! order and the same [`PlanId`], whatever order the operator entered the servers in. The
//! journal binds to that id, so a resumed run can never apply an old journal to a changed
//! topology.
//!
//! # The shape of a plan
//!
//! For every node: **enrol** the agent, apply the **hardening baseline**, **install** the
//! runtime bundle, apply the **profile** and set the node's **roles** (in that order, because
//! a profile bundle names roles too and the explicit set must win). On the primary database
//! node, **initialise the database**; on every other database node, **create the replica**
//! from the primary. Then **start services** on every node, **join** the cluster and
//! **verify health**.
//!
//! Every step that is not idempotent carries a rollback operation or a clean-up note; a
//! plan with a missing one is refused ([`DeployPlan::validate`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_mgmt_ops::{Catalogue, OpId, ParamValue};

use crate::error::WizardError;
use crate::ids::{
    self, BRANCH_EDGE_PORT, DEPLOY_AGENT_ENROL, DEPLOY_CLUSTER_JOIN, DEPLOY_DATABASE_INIT,
    DEPLOY_HARDENING_APPLY, DEPLOY_HEALTH_VERIFY, DEPLOY_RUNTIME_INSTALL, POSTGRES_PORT,
    REALTIME_PORT, WEB_PORT,
};
use crate::topology::{Host, NodeSpec, Role, Topology};

/// The domain prefix of the plan hash, so the digest is a Spotledger value and never
/// collides with a hash of another structure.
const HASH_DOMAIN: &str = "spotledger-deploy-plan-v1";

/// One typed operation a step runs: its catalogue id and its parameters, in name order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// The catalogue operation.
    pub id: OpId,
    /// The parameters, by name.
    pub params: BTreeMap<String, ParamValue>,
}

impl Operation {
    /// An operation with no parameters.
    #[must_use]
    pub fn new(id: OpId) -> Self {
        Self {
            id,
            params: BTreeMap::new(),
        }
    }

    /// The same operation with one parameter set.
    #[must_use]
    pub fn with(mut self, name: &str, value: ParamValue) -> Self {
        self.params.insert(name.to_owned(), value);
        self
    }

    /// The operation built from a constant id, failing only on a programming error in the
    /// constant (proven absent by the tests).
    fn from_text(id: &str) -> Result<Self, WizardError> {
        let id = ids::op_id(id).map_err(|error| WizardError::PlanInvalid {
            detail: format!("the crate's own operation id does not parse: {error}"),
        })?;
        Ok(Self::new(id))
    }

    /// The parameters as the JSON value a signed command carries.
    ///
    /// # Errors
    /// [`WizardError::PlanInvalid`] when a parameter value has no JSON form (a programming
    /// error: every [`ParamValue`] does).
    pub fn params_json(&self) -> Result<serde_json::Value, WizardError> {
        let mut object = serde_json::Map::new();
        for (name, value) in &self.params {
            let json = serde_json::to_value(value).map_err(|error| WizardError::PlanInvalid {
                detail: format!("parameter {name} has no JSON form: {error}"),
            })?;
            object.insert(name.clone(), json);
        }
        Ok(serde_json::Value::Object(object))
    }
}

/// What a step undoes, when it can: a compensating operation, or the note telling the
/// operator what to clean up by hand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rollback {
    /// A catalogued operation that reverses the step.
    Undo(Operation),
    /// A clean-up note for an effect no operation reverses.
    Note(String),
}

/// What kind of step this is, in the wizard's vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Enrol the node's agent with a one-time token.
    EnrolAgent,
    /// Apply the hardening baseline (WP-ADM-10).
    HardenBaseline,
    /// Install the runtime bundle.
    InstallRuntime,
    /// Apply the runtime profile as one settings diff.
    ApplyProfile,
    /// Set the node's role set.
    ConfigureRoles,
    /// Create the primary database.
    InitialiseDatabase,
    /// Build a standby from the primary.
    CreateReplica,
    /// Start the node's services.
    StartServices,
    /// Join the node to the cluster.
    JoinCluster,
    /// Verify the node's health.
    VerifyHealth,
}

impl StepKind {
    /// The stable snake-case name the console and the journal use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::EnrolAgent => "enrol_agent",
            Self::HardenBaseline => "harden_baseline",
            Self::InstallRuntime => "install_runtime",
            Self::ApplyProfile => "apply_profile",
            Self::ConfigureRoles => "configure_roles",
            Self::InitialiseDatabase => "initialise_database",
            Self::CreateReplica => "create_replica",
            Self::StartServices => "start_services",
            Self::JoinCluster => "join_cluster",
            Self::VerifyHealth => "verify_health",
        }
    }
}

/// One step of a deployment plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The step's position in the plan, from zero.
    pub index: usize,
    /// What kind of step it is.
    pub kind: StepKind,
    /// One sentence the operator reads in the progress list.
    pub title: String,
    /// The node the step runs on; `None` for a step about the deployment as a whole.
    pub host: Option<Host>,
    /// The catalogued operation the step submits.
    pub operation: Operation,
    /// The steps that must have succeeded first, in ascending order.
    pub depends_on: Vec<usize>,
    /// Whether running the step twice has the same effect as running it once.
    pub idempotent: bool,
    /// How to undo the step, or the clean-up note; `None` only for an idempotent step with
    /// nothing worth undoing.
    pub rollback: Option<Rollback>,
}

impl Step {
    /// The rollback note or operation in words, for the progress list.
    #[must_use]
    pub fn rollback_note(&self) -> Option<String> {
        match &self.rollback {
            Some(Rollback::Note(note)) => Some(note.clone()),
            Some(Rollback::Undo(operation)) => {
                Some(format!("run {} to undo it", operation.id.as_str()))
            }
            None => None,
        }
    }
}

/// The identity of a plan: the hex SHA-256 of its canonical description.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlanId(String);

impl PlanId {
    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first `len` characters of the id, for a progress title.
    #[must_use]
    pub fn short(&self, len: usize) -> &str {
        let end = len.min(self.0.len());
        &self.0[..end]
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PlanId {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// The values a plan bakes into its steps: where the data lives, which bundle to install
/// and which port the database listens on.
///
/// The defaults are the wizard's own, chosen so a three-node reference deployment needs no
/// configuration; a site with a different layout supplies its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    /// The runtime bundle to install on every node.
    pub bundle: String,
    /// The PostgreSQL port every database node uses.
    pub database_port: u16,
    /// The primary database's data directory.
    pub primary_data_dir: String,
    /// A replica's data directory.
    pub replica_data_dir: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            bundle: "spot:runtime".to_owned(),
            database_port: POSTGRES_PORT,
            primary_data_dir: "/var/lib/spotledger/pg".to_owned(),
            replica_data_dir: "/var/lib/spotledger/pg".to_owned(),
        }
    }
}

/// A deployment plan: the topology it was built for and the steps in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployPlan {
    topology: Topology,
    defaults: Defaults,
    steps: Vec<Step>,
    id: PlanId,
}

impl DeployPlan {
    /// The topology the plan deploys.
    #[must_use]
    pub fn topology(&self) -> &Topology {
        &self.topology
    }

    /// The values the steps were built with.
    #[must_use]
    pub fn defaults(&self) -> &Defaults {
        &self.defaults
    }

    /// The steps, in order.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The step at `index`, when the plan has one.
    #[must_use]
    pub fn step(&self, index: usize) -> Option<&Step> {
        self.steps.get(index)
    }

    /// How many steps the plan has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the plan has no steps (never true for a valid plan).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The plan's identity, which a journal binds to.
    #[must_use]
    pub fn id(&self) -> &PlanId {
        &self.id
    }

    /// The steps that may run now, given the steps already succeeded: their dependencies
    /// are all in `done` and they are not in `done` themselves.
    #[must_use]
    pub fn ready(&self, done: &BTreeSet<usize>) -> Vec<usize> {
        self.steps
            .iter()
            .filter(|step| {
                !done.contains(&step.index)
                    && step
                        .depends_on
                        .iter()
                        .all(|dependency| done.contains(dependency))
            })
            .map(|step| step.index)
            .collect()
    }

    /// Checks every structural rule the plan must hold: dependencies point backwards and
    /// exist, no step is listed twice, every deployment node has its enrol and hardening
    /// steps, and every non-idempotent step has a rollback.
    ///
    /// # Errors
    /// [`WizardError::PlanInvalid`] naming the first broken rule.
    pub fn validate(&self) -> Result<(), WizardError> {
        if self.steps.is_empty() {
            return Err(WizardError::PlanInvalid {
                detail: "the plan has no steps".to_owned(),
            });
        }
        for node in &self.topology.nodes {
            for kind in [StepKind::EnrolAgent, StepKind::HardenBaseline] {
                if !self
                    .steps
                    .iter()
                    .any(|step| step.kind == kind && step.host.as_ref() == Some(&node.host))
                {
                    return Err(WizardError::PlanInvalid {
                        detail: format!(
                            "{} has no {} step; the hardening baseline is never skipped",
                            node.host,
                            kind.name()
                        ),
                    });
                }
            }
            let enrol = self.steps.iter().find(|step| {
                step.kind == StepKind::EnrolAgent && step.host.as_ref() == Some(&node.host)
            });
            let harden = self.steps.iter().find(|step| {
                step.kind == StepKind::HardenBaseline && step.host.as_ref() == Some(&node.host)
            });
            if let (Some(enrol), Some(harden)) = (enrol, harden)
                && !harden.depends_on.contains(&enrol.index)
            {
                return Err(WizardError::PlanInvalid {
                    detail: format!(
                        "the hardening step on {} must depend on its enrolment",
                        node.host
                    ),
                });
            }
        }
        for (position, step) in self.steps.iter().enumerate() {
            if step.index != position {
                return Err(WizardError::PlanInvalid {
                    detail: format!("step {position} carries index {}", step.index),
                });
            }
            let mut seen = BTreeSet::new();
            for dependency in &step.depends_on {
                if *dependency >= step.index {
                    return Err(WizardError::PlanInvalid {
                        detail: format!(
                            "step {} ({}) depends on step {dependency}, which does not run before it",
                            step.index,
                            step.kind.name()
                        ),
                    });
                }
                if !seen.insert(*dependency) {
                    return Err(WizardError::PlanInvalid {
                        detail: format!("step {} lists dependency {dependency} twice", step.index),
                    });
                }
            }
            if !step.idempotent && step.rollback.is_none() {
                return Err(WizardError::PlanInvalid {
                    detail: format!(
                        "step {} ({}) is not idempotent but has no rollback",
                        step.index,
                        step.kind.name()
                    ),
                });
            }
            if let Some(Rollback::Note(note)) = &step.rollback
                && note.trim().is_empty()
            {
                return Err(WizardError::PlanInvalid {
                    detail: format!("step {} has an empty rollback note", step.index),
                });
            }
        }
        Ok(())
    }

    /// Checks that every step's operation and every parameter name exists in `catalogue`,
    /// so a plan can never submit an operation the agents do not implement.
    ///
    /// # Errors
    /// [`WizardError::PlanInvalid`] naming the first unknown operation or parameter.
    pub fn validate_catalogue(&self, catalogue: &Catalogue) -> Result<(), WizardError> {
        for step in &self.steps {
            let Some(spec) = catalogue.get(&step.operation.id) else {
                return Err(WizardError::PlanInvalid {
                    detail: format!(
                        "step {} names operation {} which the catalogue does not have",
                        step.index,
                        step.operation.id.as_str()
                    ),
                });
            };
            for name in step.operation.params.keys() {
                if !spec.params.iter().any(|param| &param.name == name) {
                    return Err(WizardError::PlanInvalid {
                        detail: format!(
                            "step {} sets parameter {name} which {} does not declare",
                            step.index,
                            step.operation.id.as_str()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// The canonical text the [`PlanId`] hashes; deterministic in the topology and steps.
    fn canonical(&self) -> String {
        let mut text = String::new();
        text.push_str(HASH_DOMAIN);
        text.push('\n');
        text.push_str(&format!(
            "profile={}\nweb_address={}\n",
            self.topology.profile.name(),
            self.topology.web_address.as_deref().unwrap_or("")
        ));
        let mut nodes: Vec<&NodeSpec> = self.topology.nodes.iter().collect();
        nodes.sort_by(|left, right| left.host.cmp(&right.host));
        for node in nodes {
            text.push_str(&format!(
                "node={} roles={} primary={}\n",
                node.host,
                node.roles
                    .iter()
                    .map(|role| role.name())
                    .collect::<Vec<_>>()
                    .join(","),
                node.primary
            ));
        }
        for step in &self.steps {
            let params = step
                .operation
                .params
                .iter()
                .map(|(name, value)| format!("{name}={value:?}"))
                .collect::<Vec<_>>()
                .join(";");
            text.push_str(&format!(
                "step={} kind={} host={} op={} params=[{}] deps={:?} idempotent={}\n",
                step.index,
                step.kind.name(),
                step.host.as_ref().map_or("", Host::as_str),
                step.operation.id.as_str(),
                params,
                step.depends_on,
                step.idempotent
            ));
        }
        text
    }
}

/// The plan id of `canonical`, as hex.
fn plan_id(canonical: &str) -> PlanId {
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    PlanId(hex)
}

/// Builds the plan for `topology` with the wizard's default values.
///
/// # Errors
/// [`WizardError::TopologyInvalid`] when the topology breaks a rule, and
/// [`WizardError::PlanInvalid`] when the built plan does not hold together (a programming
/// error, proven absent by the tests).
pub fn plan(topology: &Topology) -> Result<DeployPlan, WizardError> {
    plan_with(topology, &Defaults::default())
}

/// Builds the plan for `topology` with `defaults` baked into its steps.
///
/// # Errors
/// As [`plan`].
pub fn plan_with(topology: &Topology, defaults: &Defaults) -> Result<DeployPlan, WizardError> {
    topology.ensure_valid()?;
    let mut builder = Builder::new();

    // The plan is built node by node in host order so the same topology always produces the
    // same plan, whatever order the servers were entered in.
    let mut nodes: Vec<&NodeSpec> = topology.nodes.iter().collect();
    nodes.sort_by(|left, right| left.host.cmp(&right.host));

    let primary = topology.primary().map(|node| node.host.clone());
    let replicas: Vec<Host> = topology
        .replicas()
        .iter()
        .map(|node| node.host.clone())
        .collect();

    // Phase 1-5: per node, the agent and the software it runs.
    for node in &nodes {
        let host = node.host.clone();
        let enrol = builder.push(
            StepKind::EnrolAgent,
            Some(host.clone()),
            format!("Enrol the agent on {host}"),
            Operation::from_text(DEPLOY_AGENT_ENROL)?.with("host", ParamValue::Text(host.to_string())),
            Vec::new(),
            false,
            Rollback::Note(format!(
                "Delete the pending enrolment for {host} in the console; the token is single use, so issue a new one before retrying."
            )),
        );
        let harden = builder.push(
            StepKind::HardenBaseline,
            Some(host.clone()),
            format!("Apply the hardening baseline on {host}"),
            Operation::from_text(DEPLOY_HARDENING_APPLY)?
                .with("host", ParamValue::Text(host.to_string()))
                .with("baseline", ParamValue::Text("auto".to_owned()))
                .with("mode", ParamValue::Text("enforce".to_owned())),
            vec![enrol],
            false,
            Rollback::Note(
                "Nothing to undo: the baseline is the safe state. Re-run it at any time."
                    .to_owned(),
            ),
        );
        let install = builder.push(
            StepKind::InstallRuntime,
            Some(host.clone()),
            format!("Install the runtime bundle on {host}"),
            Operation::from_text(DEPLOY_RUNTIME_INSTALL)?
                .with("host", ParamValue::Text(host.to_string()))
                .with("bundle", ParamValue::Text(defaults.bundle.clone())),
            vec![harden],
            true,
            Rollback::Note(format!(
                "Uninstall the runtime bundle on {host} with the platform uninstaller if the deployment is abandoned."
            )),
        );
        let profile = builder.push(
            StepKind::ApplyProfile,
            Some(host.clone()),
            format!("Apply the {} profile on {host}", topology.profile),
            Operation::from_text(spot_agent_runtime::ids::RUNTIME_PROFILE_APPLY)?
                .with("profile", ParamValue::Text(topology.profile.name().to_owned())),
            vec![install],
            true,
            Rollback::Note(format!(
                "Re-apply the previous profile on {host}; the settings diff is shown before it is written."
            )),
        );
        let roles = builder.push(
            StepKind::ConfigureRoles,
            Some(host.clone()),
            format!("Set the node roles on {host}"),
            Operation::from_text(spot_agent_runtime::ids::NODE_ROLE_SET)?.with(
                "roles",
                ParamValue::List(
                    node.roles
                        .iter()
                        .map(|role| ParamValue::Text(role.name().to_owned()))
                        .collect(),
                ),
            ),
            vec![profile],
            true,
            Rollback::Note(format!(
                "Set the roles on {host} back to what they were before the deployment, if it served traffic."
            )),
        );
        builder.roles.insert(host.clone(), roles);

        if Some(&host) == primary.as_ref() {
            let init = builder.push(
                StepKind::InitialiseDatabase,
                Some(host.clone()),
                format!("Create the primary database on {host}"),
                Operation::from_text(DEPLOY_DATABASE_INIT)?
                    .with("host", ParamValue::Text(host.to_string()))
                    .with("port", ParamValue::Int(i64::from(defaults.database_port)))
                    .with(
                        "data_dir",
                        ParamValue::Text(defaults.primary_data_dir.clone()),
                    ),
                vec![roles],
                false,
                Rollback::Note(format!(
                    "Remove the new data directory on {host}; the node held no data before the deployment."
                )),
            );
            builder.database_ready.insert(host.clone(), init);
        } else if replicas.contains(&host) {
            let Some(primary_host) = primary.clone() else {
                return Err(WizardError::PlanInvalid {
                    detail: "a replica exists without a primary".to_owned(),
                });
            };
            let mut dependencies = vec![roles];
            if let Some(init) = builder.database_ready.get(&primary_host) {
                dependencies.push(*init);
            }
            dependencies.sort_unstable();
            let replica = builder.push(
                StepKind::CreateReplica,
                Some(host.clone()),
                format!("Create the replica on {host} from {primary_host}"),
                Operation::from_text(spot_agent_runtime::ids::DB_REPLICA_CREATE)?
                    .with("host", ParamValue::Text(primary_host.to_string()))
                    .with("port", ParamValue::Int(i64::from(defaults.database_port)))
                    .with("data_dir", ParamValue::Text(defaults.replica_data_dir.clone())),
                dependencies,
                false,
                Rollback::Note(format!(
                    "Stop the standby on {host} and remove its data directory; the primary is untouched."
                )),
            );
            builder.database_ready.insert(host.clone(), replica);
        }
    }

    // Phase 6: start services once each node's software and data are ready.
    let mut started: BTreeMap<Host, usize> = BTreeMap::new();
    for node in &nodes {
        let host = node.host.clone();
        let roles = builder
            .roles
            .get(&host)
            .copied()
            .ok_or_else(|| WizardError::PlanInvalid {
                detail: format!("{host} has no role step"),
            })?;
        let mut dependencies = vec![roles];
        if let Some(ready) = builder.database_ready.get(&host) {
            dependencies.push(*ready);
        }
        dependencies.sort_unstable();
        let start = builder.push(
            StepKind::StartServices,
            Some(host.clone()),
            format!("Start services on {host}"),
            Operation::from_text(spot_agent_runtime::ids::RUNTIME_SERVICE_START)?,
            dependencies,
            true,
            Rollback::Undo(
                Operation::from_text(spot_agent_runtime::ids::RUNTIME_SERVICE_STOP)?
                    .with("drain_secs", ParamValue::Duration(30)),
            ),
        );
        started.insert(host, start);
    }

    // Phase 7: join the cluster once this node and the primary database are up.
    let primary_start = primary.as_ref().and_then(|host| started.get(host).copied());
    let mut joined: BTreeMap<Host, usize> = BTreeMap::new();
    for node in &nodes {
        let host = node.host.clone();
        let mut dependencies = vec![started[&host]];
        if Some(&host) != primary.as_ref()
            && let Some(start) = primary_start
        {
            dependencies.push(start);
        }
        dependencies.sort_unstable();
        dependencies.dedup();
        let join = builder.push(
            StepKind::JoinCluster,
            Some(host.clone()),
            format!("Join {host} to the cluster"),
            Operation::from_text(DEPLOY_CLUSTER_JOIN)?
                .with("host", ParamValue::Text(host.to_string()))
                .with(
                    "primary",
                    ParamValue::Text(
                        primary
                            .as_ref()
                            .map_or_else(String::new, Host::to_string),
                    ),
                ),
            dependencies,
            false,
            Rollback::Note(format!(
                "Remove {host} from the cluster registry on the primary and clear its cluster id before retrying."
            )),
        );
        joined.insert(host, join);
    }

    // Phase 8: verify health last, once every node has joined.
    for node in &nodes {
        let host = node.host.clone();
        builder.push(
            StepKind::VerifyHealth,
            Some(host.clone()),
            format!("Verify the health of {host}"),
            Operation::from_text(DEPLOY_HEALTH_VERIFY)?
                .with("host", ParamValue::Text(host.to_string()))
                .with("deep", ParamValue::Bool(false)),
            vec![joined[&host]],
            true,
            Rollback::Note("Nothing to undo: verification changes nothing.".to_owned()),
        );
    }

    let mut plan = DeployPlan {
        topology: topology.clone(),
        defaults: defaults.clone(),
        steps: builder.steps,
        id: PlanId(String::new()),
    };
    plan.id = plan_id(&plan.canonical());
    plan.validate()?;
    Ok(plan)
}

/// Collects steps while a plan is built, remembering each node's role and readiness steps.
struct Builder {
    steps: Vec<Step>,
    roles: BTreeMap<Host, usize>,
    database_ready: BTreeMap<Host, usize>,
}

impl Builder {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            roles: BTreeMap::new(),
            database_ready: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: StepKind,
        host: Option<Host>,
        title: String,
        operation: Operation,
        mut depends_on: Vec<usize>,
        idempotent: bool,
        rollback: Rollback,
    ) -> usize {
        depends_on.sort_unstable();
        depends_on.dedup();
        let index = self.steps.len();
        self.steps.push(Step {
            index,
            kind,
            title,
            host,
            operation,
            depends_on,
            idempotent,
            rollback: Some(rollback),
        });
        index
    }
}

/// The per-role ports a plan's profile applies, for the console's summary.
#[must_use]
pub fn role_ports(role: Role) -> &'static [u16] {
    match role {
        Role::Web => &[WEB_PORT],
        Role::Jobs => &[],
        Role::Realtime => &[REALTIME_PORT],
        Role::Database => &[POSTGRES_PORT],
        Role::BranchEdge => &[BRANCH_EDGE_PORT],
    }
}

#[cfg(test)]
mod tests;
