//! Desired state and drift detection for groups of servers and tenants (WP-ADM-08.T1,
//! blueprint WS22, ADR-44).
//!
//! The Admin Center says what a group of servers and tenants **should** look like: settings,
//! package versions and service states. This crate compares that intent with what the agents
//! report, turns each difference into a risk-tagged drift with the typed catalogue operations
//! that remove it, and rolls the remediation out in canary waves that halt on failure.
//!
//! # Shape
//!
//! - [`state`]: [`DesiredState`] layers (fleet, group, server, tenant) expressed in the typed
//!   [`spot-settings`] schema, validated through its own value check and resolved through its
//!   own [`Chain`]/`resolve`; [`ObservedState`] is the agent-report seam and
//!   [`ObservedSnapshot`] one report.
//! - [`drift`]: [`detect_drift`] is pure and deterministic; each [`Drift`] carries the setting,
//!   desired and actual values, the risk from the settings spec and a remediation list of
//!   typed operations. A server silent past the deadline, or never seen, is a `Silent` drift
//!   (unknown), never healthy. [`detect_unexempted`] removes acknowledged differences, never a
//!   silence.
//! - [`plan`]: [`remediate`] builds a [`BulkPlan`] with one step per server (a single-server
//!   remediation is a plan of size one), hashed by [`BulkPlan::digest`](plan::BulkPlan::digest).
//! - [`approval`]: [`ApprovedPlan`] is the unforgeable proof that one exact plan was approved;
//!   [`Approval`] is the control plane's adapter seam, and [`ApprovalRules`] (freshness, distinct
//!   approvers, requester is not an approver) are applied by the core for every adapter.
//! - [`rehearsal`]: a structural dry run whose [`RehearsalReport`] binds to the same digest and
//!   can only be produced by [`rehearse`].
//! - [`rollout`]: the [`Rollout`] trait and [`FleetRollout`], which adapts `spot-fleet-rollout`'s
//!   canary/wave/pause state machine unchanged.
//! - [`run`]: [`BulkRun`] applies an approved, rehearsed plan through the waves and verifies
//!   each step; a failed canary halts the fleet and no later wave starts.
//! - [`exemption`]: per-server acknowledged drift with a reason and an expiry, audited.
//!
//! # The acceptance, in one sentence each
//!
//! * **Drift on one server is detected and remediated.** [`detect_drift`] finds exactly the
//!   differing settings on each reporting server, and [`remediate`] turns one drift into a
//!   one-step plan of typed operations.
//! * **A bulk change to 100 servers rolls out in waves and halts on failure.** A [`BulkRun`]
//!   starts the canary first, and a canary failure pauses the whole fleet before any later
//!   wave starts; only an explicit [`BulkRun::resume`] continues.
//!
//! # Purity
//!
//! No storage, no network, no clock, no filesystem, no floats and no `unsafe`. Time is always an
//! argument (`now` in unix microseconds for approvals and exemptions, a [`Moment`] for the
//! state machine). Secret settings are never compared, hashed or shown.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod approval;
pub mod drift;
pub mod error;
pub mod exemption;
pub mod ids;
pub mod plan;
pub mod rehearsal;
pub mod rollout;
pub mod run;
pub mod state;

pub use approval::{Approval, ApprovalDecision, ApprovalRules, ApprovedPlan, MemoryApprovals};
pub use drift::{
    Drift, DriftSubject, ServerDrift, detect_drift, detect_unexempted, group_by_server,
};
pub use error::{Error, codes};
pub use exemption::{
    AuditEvent, AuditSink, Exemption, ExemptionAction, ExemptionBook, ExemptionRequest, MemoryAudit,
};
pub use ids::{PLAN_DOMAIN, PlanDigest, SettingKey};
pub use plan::{
    BulkPlan, PlanStep, RemediationPolicy, SkippedDrift, TypedOperation, operation, remediate,
    risk_name,
};
pub use rehearsal::{RehearsalProblem, RehearsalReport, rehearse};
pub use rollout::{
    ConcurrencyLimit, Epoch, FailureKind, Fleet, FleetNode, FleetRollout, Health, Moment,
    NodeFailure, NodeId, Observations, Overlap, PauseOn, PauseReason, PauseRecord, Percent,
    ReleaseId, ResolvedWave, Rollout, RolloutPlan, Seed, Selector, Soak, SuccessCriteria, Wave,
    WaveAction, WaveSize, WaveState,
};
pub use run::{BulkRun, RunAction, RunGuards, StepFailure, StepRunner};
pub use state::{
    DesiredFleet, DesiredState, EffectiveDesired, InMemoryObserved, Liveness, ObservedSnapshot,
    ObservedState, OperationSet, ServiceState, liveness,
};

/// The settings scope vocabulary a desired-state layer is built from (re-exported so a caller
/// can construct a layer without importing `spot-settings` directly).
pub use spot_settings::{Difference, Scope, ScopeRef};

/// The management vocabulary every remediation step is made of (re-exported so a caller builds
/// a desired state and reads a plan without importing `spot-mgmt-ops` directly).
pub use spot_mgmt_ops::{OpId, ParamValue, Risk};

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-desired-state";

/// The crate version, for diagnostics and the error catalogue's `since` rows.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
