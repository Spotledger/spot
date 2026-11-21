//! Deployment self-awareness (WP-AI-02.T1, blueprint WS13, ADR-26).
//!
//! An agent's knowledge must never lag the system it acts on. This crate is the deployment
//! side of that rule: a **live, versioned model of how this installation is deployed** —
//! edition, platform, topology (standalone or cluster, head office or branch edge, the roles
//! each node serves), backend (PostgreSQL version and replicas), cache and coordination, sync
//! profile, runtime version, installed apps, seam overrides, the last migration runs,
//! connectivity, site policies, and the agent's own tier.
//!
//! # Built from ground truth, moved by events
//!
//! [`Facts`] is what the authoritative sources say. [`FactSource`] is the read side:
//! [`adapters`] wrap `spot-installer`, `spot-settings`, `spot-agent-runtime`,
//! `spot-fleet-health`, `spot-seams` and `spot-agent-tiers`. [`refresh`] merges the sources,
//! records each source's version, and builds the model at the next epoch — so no fact is ever
//! cached without one.
//!
//! [`DeploymentEvent`] is the change side. It reuses WP-AI-01's `SystemEvent`: a migration
//! record, an app install and an app removal convert directly
//! ([`DeploymentEvent::from_system_event`]), and the control plane's topology, licence and
//! connectivity events join the same stream. [`apply`] is pure: it clones the model, applies
//! the change, validates the result and returns it. An event names the epoch it produces;
//! duplicates are ignored by id, older epochs are stale, a gap is reported
//! ([`ApplyOutcome::Gap`]) and asks the caller to refresh rather than guess.
//!
//! # An agent's view
//!
//! [`ModelHandle`] is what an agent holds: a cheap clone over a `tokio::sync::watch` channel.
//! [`ModelHandle::current`] reads the newest model; [`ModelHandle::subscribe`] wakes on every
//! new epoch; [`ModelHandle::self_check`] verifies the model against the sources and refreshes
//! on the first mismatch. [`EpochStamp`] records the epoch and digest an action relied on in an
//! audit entry ([`EpochStamp::attach`]).
//!
//! # Purity
//!
//! No storage, no network, no wall clock (time is always an argument), no floats, no `unsafe`,
//! no `unwrap`/`expect` outside tests. A deployment fact is never a secret: a connection
//! string, password or key cannot enter the model, and the model's `Debug` and `Serialize`
//! forms carry only what the adapters read.
//!
//! ```
//! use std::collections::BTreeMap;
//!
//! use spot_agent_tiers::Tier;
//! use spot_deployment_model::*;
//!
//! # fn main() -> Result<(), spot_deployment_model::DeploymentError> {
//! let facts = Facts {
//!     edition: Some(Edition::new("enterprise")?),
//!     platform: Some(Platform::new(OsFamily::Linux, "Ubuntu 24.04")?),
//! #   topology: Some(Topology::new(
//! #       TopologyMode::Standalone,
//! #       Placement::HeadOffice,
//! #       BTreeMap::from([(
//! #           "ln-1".to_owned(),
//! #           DeploymentNode::new([NodeRole::Web, NodeRole::Database])?,
//! #       )]),
//! #   )?),
//! #   backend: Some(Backend::new(
//! #       BackendEngine::Postgres,
//! #       "16.4",
//! #       vec![Replica::new("ln-1", ReplicaRole::Primary, false)?],
//! #   )?),
//! #   coordination: Some(Coordination::new(CacheBackend::Database, Coordinator::InProcess)),
//! #   sync: Some(SyncProfile::standalone()),
//! #   runtime_version: Some("0.1.0".to_owned()),
//! #   apps: Some(BTreeMap::new()),
//! #   overrides: Some(Vec::new()),
//! #   migrations: Some(Vec::new()),
//! #   connectivity: Some(Connectivity::new(true, Some(1_000_000))),
//! #   policies: Some(SitePolicies::default()),
//!     agent_tier: Some(Tier::T1),
//! };
//! let source = MemoryFacts::with_facts(1, facts);
//! let model = refresh(None, &[&source], 1_000)?;
//! assert_eq!(model.epoch, 1);
//! assert_eq!(model.edition.as_str(), "enterprise");
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]
#![cfg_attr(test, allow(clippy::expect_used))]

pub mod adapters;
mod canonical;
pub mod error;
pub mod event;
pub mod facts;
pub mod handle;
pub mod model;
pub mod types;

pub use adapters::{
    AgentFacts, FleetFacts, InstallerFacts, RuntimeFacts, SeamsFacts, SettingsFacts,
};
pub use error::{DeploymentError, codes};
pub use event::{
    ApplyOutcome, ConfigChange, DeploymentEvent, DeploymentEventKind, apply, parse_override_kind,
    parse_replay_policy, sorted_overrides,
};
pub use facts::{FactSource, Facts, MemoryFacts, Mismatch, SelfCheck, read, refresh, verify};
pub use handle::{DIGEST_PAYLOAD_KEY, EPOCH_PAYLOAD_KEY, EpochStamp, HandleOutcome, ModelHandle};
pub use model::DeploymentModel;
pub use spot_self_model::{ChangeKind, EvidenceRef, EvidenceSource};
pub use types::{
    AppEntry, AppState, ApprovalPolicy, Backend, BackendEngine, CacheBackend, Connectivity,
    Coordination, Coordinator, DeploymentNode, Digest, EPOCH_LEDGER_LIMIT, Edition, EpochEntry,
    FORMAT, FactSourceKind, MIGRATION_HISTORY_LIMIT, MigrationRun, NodeRole, OsFamily, Override,
    OverrideKind, Placement, Platform, ReplayPolicy, Replica, ReplicaRole, SEEN_EVENT_LIMIT,
    SitePolicies, SyncMode, SyncProfile, Topology, TopologyMode,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-deployment-model";
