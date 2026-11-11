//! `spot-deploy-wizard` — the guided deployment engine behind the Admin Center's "Add
//! server" wizard (WP-ADM-13.T1, blueprint WS22, ADR-44).
//!
//! The wizard asks three things: *which servers*, *which roles they serve* and *which
//! profile*. This crate turns those answers into a deployment that a non-CLI administrator
//! can watch, pause and resume. The screens themselves are WP-ADM-13.T2; everything here is
//! the engine behind them.
//!
//! # Shape
//!
//! - [`topology`]: [`Topology`] as data — nodes, roles and profile — and [`Topology::validate`],
//!   which reports **every** broken rule at once, each with the fix in plain words. Roles and
//!   profiles are the agent runtime's own vocabulary (`web`, `jobs`, `realtime`, `database`,
//!   `branch-edge`; `standalone`, `cluster-basic`, `cluster-redis`).
//! - [`facts`] and [`preflight`]: the agent's report ([`facts::AgentFacts`]) and the checks
//!   computed from it — supported OS, disk, memory, time sync, clock skew, free ports,
//!   connectivity between nodes — with severities. [`preflight::Preflight::ensure_ready`]
//!   refuses to let a plan start on a blocking finding.
//! - [`plan`]: [`plan::plan`] turns a validated topology into a [`plan::DeployPlan`]: ordered
//!   [`plan::Step`]s as catalogued operations — enrol, harden, install, profile, roles,
//!   database, replica, start, join, verify — with dependencies that say what runs in
//!   parallel, and a rollback operation or clean-up note for every step that needs one. The
//!   plan has a [`plan::PlanId`] that binds it to its journal.
//! - [`journal`]: the durable progress journal ([`journal::Journal`], with a memory
//!   implementation for tests) and [`journal::resume`], which says which step to retry after
//!   a closed laptop or a failed step, and whether a non-idempotent step needs its clean-up
//!   first.
//! - [`executor`]: [`executor::DeployExecutor`], the seam that carries a command the console
//!   signed to a node; [`executor::run`] drives a plan through it; [`executor::SimulatedExecutor`]
//!   records commands and injects failures for tests.
//! - [`estimate`]: the duration table, [`estimate::estimate`] (critical path, so parallelism
//!   is real) and [`estimate::progress`] for the wizard's progress screen.
//! - [`catalogue`]: the six `deploy.*` operations the wizard adds to the management
//!   catalogue, so every step has an equivalent command and the agent applies its own
//!   verification.
//!
//! # Purity
//!
//! No clock is read (every instant is an argument), no float is used for a duration or a
//! quantity (the project coding rules, ADR-12), there is no `unsafe`, no `unwrap` outside tests,
//! and nothing here touches a socket, a file or the host. The console supplies the facts,
//! the signatures and the durable journal; this crate decides what is valid, what runs and
//! in what order.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::float_arithmetic)]

pub mod audit;
pub mod authz;
pub mod catalogue;
pub mod error;
pub mod estimate;
pub mod executor;
pub mod facts;
pub mod ids;
pub mod journal;
pub mod plan;
pub mod preflight;
pub mod signed_facts;
pub mod topology;

pub use audit::{AuditEntry, AuditOutcome, AuditSink, AuditUnavailable, MemoryAudit};
pub use authz::{
    APPROVAL_WINDOW_SECS, AdminIdentity, Approval, Authorised, DEPLOY_PERMISSION, PlanHash,
    STEP_UP_WINDOW_SECS, StepUp, authorise,
};
pub use catalogue::{WizardCatalogue, build_catalogue};
pub use error::WizardError;
pub use estimate::{Estimate, Progress, Speed, base_secs, estimate, estimate_after, progress};
pub use executor::{
    CommandWindow, DeployExecutor, ExecutorFailure, FailedStep, MAX_COMMAND_LIFETIME_SECS, Receipt,
    RunEnv, RunReport, SimulatedExecutor, Submitted, run,
};
pub use facts::{AgentFacts, OsFacts, OsFamily, PeerFacts, TimeFacts};
pub use journal::{
    Journal, JournalEntry, MAX_DETAIL_CHARS, MemoryJournal, ResumePoint, StepState,
    completed_steps, resume, sanitize_detail,
};
pub use plan::{
    Defaults, DeployPlan, Operation, PlanId, Rollback, Step, StepKind, plan, plan_with,
};
pub use preflight::{Check, CheckKind, Preflight, Severity, preflight, required_ports};
pub use signed_facts::{
    DEFAULT_MAX_FACTS_AGE_SECS, EnrolledAgents, FactsLedger, FactsPolicy,
    MAX_FACTS_AGE_CEILING_SECS, MemoryFactsLedger, ServerFacts, SignedFacts,
};
pub use topology::{Host, HostError, NodeSpec, Profile, Role, Rule, Topology, Violation};

use spot_mgmt_ops::Catalogue;

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-deploy-wizard";

/// Where one fact report came from, as the plan remembers it: enough to refuse a run when the
/// report has aged, and to make the plan hash depend on the facts it was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactStamp {
    /// The node the report is about.
    pub host: Host,
    /// The agent that signed it.
    pub agent_id: String,
    /// The report's sequence.
    pub sequence: u64,
    /// When the agent observed the facts.
    pub observed_at: i64,
    /// The oldest the report may be when a run starts, in seconds.
    pub max_age_secs: i64,
}

/// Everything the wizard needs before a run: the plan, the pre-flight report it passed and
/// the steps' catalogue validation.
///
/// [`prepare`] is the one call a console makes when the operator presses "Deploy": every
/// refusal arrives before anything is signed or submitted. Running it needs an
/// [`Authorised`] token for exactly this plan ([`authz::authorise`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    plan: DeployPlan,
    preflight: Preflight,
    stamps: Vec<FactStamp>,
    plan_hash: PlanHash,
}

impl Prepared {
    /// The deployment plan.
    #[must_use]
    pub fn plan(&self) -> &DeployPlan {
        &self.plan
    }

    /// The pre-flight report; it has no blocking check, or [`prepare`] would have refused.
    #[must_use]
    pub fn preflight(&self) -> &Preflight {
        &self.preflight
    }

    /// The hash an approval names together with the plan id: it covers the plan id and the
    /// stamp (agent, sequence, observation time) of every fact the plan was built from.
    #[must_use]
    pub fn plan_hash(&self) -> &PlanHash {
        &self.plan_hash
    }

    /// The facts the plan was built from, with when each was observed.
    #[must_use]
    pub fn fact_stamps(&self) -> &[FactStamp] {
        &self.stamps
    }

    /// Runs the prepared plan with the authority `authorised` and the services in `env`.
    ///
    /// # Errors
    /// As [`executor::run`]: a token for another plan or one that lapsed, a fact that aged
    /// out, an audit entry that could not be written, or a plan, journal or command problem.
    pub fn run(
        &self,
        authorised: &Authorised,
        env: &mut RunEnv<'_>,
    ) -> Result<RunReport, WizardError> {
        executor::run(self, authorised, env)
    }
}

fn hash_plan(id: &PlanId, stamps: &[FactStamp]) -> PlanHash {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut text = format!("spot.deploy-wizard.plan-hash.v1\nplan={}\n", id.as_str());
    for stamp in stamps {
        let _ = writeln!(
            text,
            "fact={}|{}|{}|{}",
            stamp.host.as_str(),
            stamp.agent_id,
            stamp.sequence,
            stamp.observed_at
        );
    }
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(text.as_bytes()) {
        let _ = write!(hex, "{byte:02x}");
    }
    PlanHash::from_hex(hex)
}

/// Validates `topology`, runs pre-flight over verified `facts` and proves every step names a
/// catalogued operation in `catalogue`.
///
/// The facts are [`ServerFacts`]: only [`SignedFacts::verify`] builds them, so a forged,
/// stale or replayed report never reaches the planner.
///
/// # Errors
/// [`WizardError::TopologyInvalid`], [`WizardError::PreflightBlocked`] or
/// [`WizardError::PlanInvalid`], in that order.
pub fn prepare(
    topology: &Topology,
    facts: &[ServerFacts],
    catalogue: &Catalogue,
) -> Result<Prepared, WizardError> {
    let plan = plan::plan(topology)?;
    plan.validate_catalogue(catalogue)?;
    let reports: Vec<AgentFacts> = facts.iter().map(|f| f.facts().clone()).collect();
    let preflight = Preflight::for_topology(topology, &reports);
    preflight.ensure_ready()?;
    let mut stamps: Vec<FactStamp> = facts
        .iter()
        .map(|f| FactStamp {
            host: f.facts().host.clone(),
            agent_id: f.agent_id().to_owned(),
            sequence: f.sequence(),
            observed_at: f.observed_at(),
            max_age_secs: f.max_age_secs(),
        })
        .collect();
    stamps.sort_by(|a, b| a.host.cmp(&b.host));
    let plan_hash = hash_plan(plan.id(), &stamps);
    Ok(Prepared {
        plan,
        preflight,
        stamps,
        plan_hash,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn node(host: &str, roles: &[Role]) -> NodeSpec {
        NodeSpec::new(host, roles).unwrap()
    }

    fn topology() -> Topology {
        Topology::new(
            vec![
                node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                node("db1.example.com", &[Role::Database]),
                node("db2.example.com", &[Role::Database]),
            ],
            Profile::ClusterBasic,
        )
        .with_web_address("erp.example.com")
    }

    fn facts(topology: &Topology) -> Vec<ServerFacts> {
        topology
            .nodes
            .iter()
            .map(|node| {
                let peers = topology
                    .nodes
                    .iter()
                    .filter(|peer| peer.host != node.host)
                    .map(|peer| PeerFacts {
                        host: peer.host.clone(),
                        reachable: true,
                    })
                    .collect();
                AgentFacts::new(
                    node.host.clone(),
                    OsFacts {
                        family: OsFamily::Linux,
                        release: "Ubuntu 24.04 LTS".to_owned(),
                        supported: true,
                    },
                )
                .with_peers(peers)
            })
            .map(|facts| ServerFacts::for_testing(facts, 1_700_000_000))
            .collect()
    }

    fn catalogue() -> Catalogue {
        use spot_mgmt_ops::{CatalogueBuilder, OperationModule};
        let mut builder = CatalogueBuilder::new();
        catalogue::WizardCatalogue.register(&mut builder);
        spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
        builder.build().unwrap()
    }

    #[test]
    fn prepare_brings_a_healthy_topology_to_a_runnable_plan() {
        let topology = topology();
        let prepared = prepare(&topology, &facts(&topology), &catalogue()).unwrap();
        assert!(prepared.preflight().is_ready());
        assert_eq!(prepared.plan().topology(), &topology);
        assert!(!prepared.plan().is_empty());
    }

    #[test]
    fn prepare_refuses_a_blocking_fact_before_anything_runs() {
        let topology = topology();
        let mut facts = facts(&topology);
        facts[1] = ServerFacts::for_testing(
            facts[1].facts().clone().with_data_dir_free_gb(1),
            1_700_000_000,
        );
        let error = prepare(&topology, &facts, &catalogue()).unwrap_err();
        assert_eq!(error.code(), error::PREFLIGHT_BLOCKED);
    }

    #[test]
    fn prepare_refuses_a_broken_topology_first() {
        let broken = Topology::new(
            vec![node("app1.example.com", &[Role::Web])],
            Profile::ClusterBasic,
        );
        let error = prepare(&broken, &[], &catalogue()).unwrap_err();
        assert_eq!(error.code(), error::TOPOLOGY_INVALID);
    }
}
