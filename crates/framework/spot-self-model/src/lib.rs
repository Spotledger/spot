//! The system self-model (WP-AI-01.T1, blueprint WS13, ADR-26).
//!
//! The self-model is an in-memory graph of what the platform is **made of** and **how it
//! changed**: DocTypes and fields, scripts and the fields they name, seams and the steps that
//! run at each hook point, installed apps and their patches, migration runs, roles, duties,
//! permissions, jobs, schedules, settings and runtime health.
//!
//! It is built from the platform's authoritative sources, never from a separate store of
//! opinions. Each source is a [`SourceAdapter`]: metadata and customisations (`spot-meta`),
//! the step and seam registry (`spot-seams`), installed apps and manifests
//! (`spot-installer`, `spot-manifest`), migration runs and release metadata
//! (`spot-migration-audit`, `spot-release-meta`), the permission snapshot
//! (`spot-permissions`), the script index (`spot-ts`) and runtime health
//! (`spot-fleet-health`, telemetry).
//!
//! # Answers, not guesses
//!
//! [`SelfModelService::why`] walks causal edges from a change or symptom towards its causes:
//! a field's options changed <- an override from customisation X <- a bundle promoted by
//! migration Y, or a validation error <- a script that hooks the step <- the app that
//! installed it <- the upgrade record. Every returned step carries the [`EvidenceRef`] that
//! proves it, and an edge with no evidence is never walked.
//!
//! # Permission awareness
//!
//! Introspection is read-only. Each query is filtered by the caller's
//! [`Tier`](spot_agent_tiers::Tier): T0 sees metadata and health, while roles, duties,
//! permissions and settings need T1. A query for a node the caller's tier does not reach is
//! answered exactly like a query for a node the snapshot does not hold, so a caller cannot learn
//! which items exist; the tier refusal is recorded on the [`QueryAudit`] sink for the owner
//! instead (see the [`service`] module). Every query, allowed or refused, is recorded through
//! that sink.
//!
//! [`QueryAudit`]: service::QueryAudit
//!
//! # Purity
//!
//! No storage, no network, no clock (time is always an argument), no floats, no `unsafe`, no
//! `unwrap`/`expect` outside tests.
//!
//! ```
//! use spot_agent_tiers::Tier;
//! use spot_self_model::adapters::{HealthAdapter, HealthSignalRecord, SettingRecord, SettingsAdapter};
//! use spot_self_model::{
//!     Caller, ChangeKind, EvidenceRef, EvidenceSource, MemoryAudit, NodeKind, NodeRef, SelfModel,
//!     SelfModelService,
//! };
//!
//! let settings = SettingsAdapter::new().with_setting(SettingRecord {
//!     key: "job.retry_limit".to_owned(),
//!     scope: "tenant:acme".to_owned(),
//!     at: 390,
//!     change: "the retry limit was lowered".to_owned(),
//!     change_kind: ChangeKind::Updated,
//!     cause: None,
//!     evidence: EvidenceRef::at(EvidenceSource::Settings, "setting:1", 390),
//! });
//! let health = HealthAdapter::new().with_signal(HealthSignalRecord {
//!     name: "job failing:nightly".to_owned(),
//!     status: "failing".to_owned(),
//!     target: NodeRef::new(NodeKind::Job, "nightly"),
//!     cause: Some(NodeRef::new(NodeKind::Setting, "job.retry_limit")),
//!     at: 400,
//!     detail: "the nightly job is failing".to_owned(),
//!     evidence: EvidenceRef::at(EvidenceSource::Telemetry, "health:1", 400),
//! });
//! let model = SelfModel::build(7, &[&settings, &health])?;
//! let mut audit = MemoryAudit::new();
//! let mut service = SelfModelService::new(&model, &mut audit);
//! let explanation = service.why(
//!     &Caller::new("agent:t1", Tier::T1),
//!     &NodeRef::new(NodeKind::Job, "nightly"),
//!     1_000,
//! )?;
//! assert_eq!(explanation.paths[0].steps.len(), 3);
//! # Ok::<(), spot_self_model::SelfModelError>(())
//! ```

#![forbid(unsafe_code)]

pub mod adapters;
pub mod error;
pub mod event;
pub mod evidence;
pub mod kind;
pub mod model;
pub mod service;

pub use adapters::{
    AppInstallRecord, AppsAdapter, AssignmentRecord, FleetFinding, HealthAdapter,
    HealthSignalRecord, MetadataAdapter, MigrationImpact, MigrationsAdapter, PatchRecord,
    PermissionsAdapter, ScriptRecord, ScriptsAdapter, SeamsAdapter, SettingRecord, SettingsAdapter,
};
pub use error::{SelfModelError, codes};
pub use event::{SystemEvent, permission_ref, pipeline_step_ref};
pub use evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry, Provenance, TimeRange};
pub use kind::{CauseDirection, EdgeKind, NodeKind, SourceKind};
pub use model::{
    Edge, EdgeId, Node, NodeId, NodeRef, SelfModel, SourceAdapter, SourceData, node_id, node_key,
};
pub use service::{
    Caller, CausePath, CauseStep, Description, Direction, Explanation, MemoryAudit, Neighbour,
    QueryAudit, QueryKind, QueryOutcome, QueryRecord, SelfModelService, WhyOptions,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-self-model";
