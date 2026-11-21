//! `spot-iam-governance` — access governance as a pure, deterministic core
//! (WP-IAM-08.T1; ADR-070 section 8).
//!
//! | Area | Where |
//! | --- | --- |
//! | The grant request, its idempotent id, the grantor, the override | [`GrantRequest`], [`GrantRequestId`], [`Grantor`], [`OverrideSubmission`], [`MitigatingControl`] |
//! | The scope a grant carries and its hierarchy | [`GrantScope`], [`ScopeHierarchy`] |
//! | The tenant policy: delegation and approval | [`DelegationPolicy`], [`ApprovalPolicy`] |
//! | The decision and the gate | [`GrantDecision`], [`GrantGate`], [`GovernanceState`] |
//! | The auditable override record | [`SodOverrideRecord`], [`record_overrides`] |
//! | Grant approval on the BPM engine | [`ApprovalSpec`], [`approval_process`], [`ProcessLauncher`], [`GrantApproval`] |
//! | The permission-change audit and the between-two-dates diff | [`ChangeEvent`], [`Snapshot`], [`diff_access`], [`AccessDiff`] |
//!
//! # Behaviour (ADR-070 section 8, in our own words)
//!
//! - A grant is evaluated **before the assignment takes effect**. The gate first applies the
//!   delegated-administration limit (a grantor grants only roles and scopes they hold, and
//!   never a manual change to an IdP-managed role), then the segregation-of-duties check over
//!   the user's resulting duties in the resulting scope, then the tenant's approval policy.
//! - A segregation-of-duties conflict **blocks** the grant unless the grantor holds the
//!   `override_sod` right and records a non-blank reason and a [`MitigatingControl`]. The
//!   override is an auditable [`SodOverrideRecord`] covering exactly one (rule, user, scope),
//!   and it is the note an access review shows.
//! - Approval is a small [`ProcessVersion`] built from the `spot-bpm-model`
//!   approval-with-escalation template and launched through a [`ProcessLauncher`] the host
//!   implements over the BPM engine. The grant takes effect only on approval; a rejection or
//!   expiry leaves nothing granted; the request id is the idempotency key throughout.
//! - Every role, duty and grant change is a [`ChangeEvent`] with canonical before/after
//!   [`Snapshot`]s, the actor (and `via`), a reason and the `identity` topic it is emitted
//!   through. [`diff_access`] answers "what changed between two dates".
//! - "Including descendants" is resolved with the caller-supplied [`ScopeHierarchy`]; a grant
//!   covering a value and its descendants is expanded into one scope token per covered value,
//!   so it conflicts exactly with a grant sharing any of them.
//!
//! # Purity
//!
//! No storage, no clock, no network, no ambient reads, no floats, no `unsafe` and no
//! `unwrap`/`expect` outside tests (the project coding rules). Instants are arguments.

#![forbid(unsafe_code)]

pub mod approval;
pub mod audit;
pub mod conflict;
pub mod error;
pub mod gate;
pub mod override_record;
pub mod policy;
pub mod request;
pub mod scope;
pub mod target;

pub use approval::{
    APPROVAL_PROCESS_APP, APPROVAL_PROCESS_NAME, ApprovalDecision, ApprovalSpec, ApprovalState,
    GrantApproval, LaunchReceipt, ProcessLauncher, approval_process,
};
pub use audit::{
    AccessDiff, ChangeActor, ChangeEvent, ChangeKind, ChangeOp, Snapshot, diff_access,
};
pub use conflict::{Conflict, Refusal};
pub use error::GovError;
pub use gate::{GovernanceState, GrantDecision, GrantGate};
pub use override_record::{SodOverrideRecord, record_overrides};
pub use policy::{ApprovalPolicy, DelegationPolicy};
pub use request::{GrantRequest, GrantRequestId, Grantor, MitigatingControl, OverrideSubmission};
pub use scope::{GrantScope, ScopeHierarchy};
pub use target::{EscalationTarget, MAX_TARGET_BYTES};

/// The stable error codes of this crate, as listed in the error catalogue.
pub mod codes {
    pub use crate::error::{
        APPROVAL_NOT_PENDING, APPROVAL_PROCESS, APPROVER_IS_GRANTEE, DELEGATION_DENIED,
        IDP_MANAGED, NOT_APPROVED, NOT_THE_APPROVER, OVERRIDE_INCOMPLETE, OVERRIDE_NOT_PERMITTED,
        REQUEST_INVALID, SNAPSHOT_FAILED,
    };
}

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-iam-governance";
