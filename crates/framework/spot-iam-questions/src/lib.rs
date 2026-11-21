//! `spot-iam-questions` — emergency access with controller review and the four answerable
//! questions, as pure, clock-free cores (WP-IAM-08.T2; ADR-070 sections 8.5 and 9.1).
//!
//! The four questions are product features, not reports someone builds later. Each is a
//! permission-aware, audited query over a read model, and each row carries the rule that
//! produced it.
//!
//! | Area | Where |
//! | --- | --- |
//! | The read model the questions evaluate | [`AccessModel`], [`DocumentRecord`], [`ShareRecord`], [`Change`] |
//! | The four questions | [`Questions`], [`PermissionRow`], [`Holder`], [`Viewer`], [`Change`] |
//! | Paging and audit | [`Page`], [`Paged`], [`QueryAudit`], [`QueryAuditor`], [`MemoryAuditor`] |
//! | Emergency access | [`EmergencyGrant`], [`EmergencyPolicy`], [`ReviewTask`], [`SessionContext`] |
//! | The session recorder (shared with break-glass) | [`SessionRecorder`] |
//!
//! # The four questions (ADR-070 section 8, rule 5)
//!
//! - **What can this user do and why** — [`Questions::what_can_user_do`]: every effective
//!   permission with the assignment, role, duty, source, scope and the decision core's exact
//!   rule.
//! - **Who can do this and why** — [`Questions::who_can_do`]: the same why-chain, for every
//!   holder of one permission in a scope.
//! - **Who can see this document** — [`Questions::who_can_see`]: the read filter the query
//!   machinery (`spot-perm-query`) compiles, evaluated over the document, with shares and the
//!   field-level groups each viewer may read.
//! - **What changed between two dates** — [`Questions::access_changes`]: the permission-change
//!   audit, flattened and deliberately stripped of any before/after snapshot.
//!
//! Every answer is itself permission-checked (only a holder of the access-governance duty may
//! ask about others, or about document access and history at all) and audited through a
//! [`QueryAuditor`]; results never include a field value.
//!
//! # Emergency access (ADR-070 section 9, rule 1)
//!
//! An [`EmergencyGrant`] is a time-boxed assignment with source `emergency`: a request with a
//! reason, the role owner's approval on the BPM engine (through the same `ProcessLauncher`
//! trait T1 uses), full action recording into the shared `spot-break-glass` component, and on
//! the end a [`ReviewTask`] for a controller who is **not the user**. When the controller is
//! the approver, the policy may require a second, distinct check before the review confirms.
//! An unconfirmed review escalates on schedule through the engine's `TimerPort`.
//!
//! # Purity
//!
//! No storage, no clock, no network, no ambient reads, no floats, no `unsafe` and no
//! `unwrap`/`expect` outside tests (the project coding rules). Instants are arguments.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod audit;
pub mod changes;
pub mod emergency;
pub mod engine;
pub mod error;
pub mod explain;
pub mod model;
pub mod page;
pub mod questions;
pub mod recording;
pub mod visibility;

pub use audit::{
    EmergencyAuditor, EmergencyRefusal, MemoryAuditor, MemoryEmergencyAuditor, NoAudit, QueryAudit,
    QueryAuditor,
};
pub use changes::changes;
pub use emergency::{
    EMERGENCY_REVIEW_NODE, EmergencyGrant, EmergencyPhase, EmergencyPolicy, EmergencyRequest,
    Escalation, ReviewCheck, ReviewState, ReviewTask, SessionAction, SessionContext,
};
pub use engine::{ScopeQuery, ScopeValue, World, build, build_guarded, epoch, read_target};
pub use error::QuestionsError;
pub use explain::{Holder, PermissionRow, PermissionSpec, action_of};
pub use model::{AccessModel, Change, DocumentRecord, ShareRecord};
pub use page::{DEFAULT_PAGE, MAX_PAGE, Page, Paged};
pub use questions::{ACCESS_GOVERNANCE_DUTY, AskPolicy, Questions};
pub use recording::SessionRecorder;
pub use visibility::{FieldVisibility, Viewer};

/// The stable error codes of this crate, as listed in the error catalogue.
pub mod codes {
    pub use crate::error::{
        APPROVAL, EMERGENCY_DURATION, EMERGENCY_STATE, ESCALATION_TARGET_INVALID, MODEL_INVALID,
        NOT_PERMITTED, PAGE_INVALID, RECORDING, REQUEST_INVALID, REVIEW_SELF, REVIEW_WRONG_PERSON,
        UNKNOWN_DOCTYPE, UNKNOWN_PERMISSION,
    };
}

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-iam-questions";
