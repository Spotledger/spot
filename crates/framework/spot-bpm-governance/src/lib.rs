//! `spot-bpm-governance` — governance for process-first operation (WS8, WP-BPM-08.T1; ADR-49,
//! ADR-080).
//!
//! A record type may be **governed**: created only inside a running request, at the step that is
//! allowed to create it. Governance is **off by default** and configured per company and record
//! type, so turning it on for one company changes nothing for another. This crate is the decision
//! and its audit; the document engine is the enforcement point.
//!
//! # The rule
//!
//! [`GovernanceGuard::before_create`] decides one create attempt:
//!
//! 1. mode `free` (the default): allowed, nothing audited;
//! 2. mode `governed`: allowed when the attempt carries a request step the authority has
//!    **authenticated** for the acting person and the current attempt, and whose node permits the
//!    record type, **or** when the principal holds the `bpm.outside_process` permission and
//!    supplied a reason of at least the configured length — and that exception is audited;
//!    otherwise refused, and the refusal is audited;
//! 3. mode `process_only` (ADR-080 section 2.3): this crate offers **no** permission or override
//!    route and defers to the entry crate (WP-BPM-13).
//!
//! [`GovernanceController`] is the engine hook: a `before_create` step, so every channel that
//! creates a record — the native API, an import, a server script, a process step — is subject to
//! the same rule, because they all run the record's own create lifecycle. Nothing is enforced in
//! the UI.
//!
//! # Where the policy comes from
//!
//! The mode is a setting ([`settings::POLICY_KEY`]) and the reason floor another
//! ([`settings::REASON_MIN_KEY`]), both read through the `spot-settings` API
//! ([`settings::SettingsPolicy`]), never from a store directly. A change is effective-dated:
//! records created before its effective instant keep the earlier mode
//! ([`policy::PolicyChange`]), and every change is audited.
//!
//! # The audit
//!
//! Every exception and every refusal writes a [`GovernanceAudit`] entry — who, what, why and
//! which policy — in the tamper-evident shape of `spot-audit-chain`. A transaction-bound
//! [`AuditSink`] writes it through [`enqueue_governance`], so the entry rides the outbox in the
//! same transaction as the record write: a refusal or an exception cannot survive without its
//! audit, and cannot be lost with it.
//!
//! # Purity
//!
//! No storage, no clock (time is an argument), no network, no floats, no `unsafe`, and no
//! `unwrap`/`expect` outside tests (the project coding rules).

#![forbid(unsafe_code)]

pub mod audit;
pub mod authority;
pub mod codes;
pub mod context;
pub mod controller;
pub mod decision;
pub mod error;
pub mod guard;
pub mod mode;
pub mod permission;
pub mod policy;
pub mod settings;

pub use audit::{
    AuditKind, AuditSink, FailingAudit, GOVERNANCE_SCHEMA_VERSION, GOVERNANCE_TOPIC,
    GovernanceAudit, RecordingAudit, enqueue_governance, outbox_event_of,
};
pub use authority::{MemoryRequests, NoRequests, RequestAuthority, VerifiedRequest};
pub use context::{CaseContext, actor_text};
pub use controller::{GovernanceController, REASON_FIELD, STEP_NAME};
pub use decision::{DeferredTo, GovernanceDecision, RefusalReason, Route};
pub use error::GovernanceError;
pub use guard::{CreateAttempt, GovernanceGuard};
pub use mode::GovernanceMode;
pub use permission::{
    MemoryPrincipals, NoOutsideProcess, OUTSIDE_PROCESS_PERMISSION, OutsideProcess,
    PrincipalSource, RoleOutsideProcess,
};
pub use policy::{GovernancePolicy, MemoryPolicyStore, PolicyChange, PolicyStore, effective_mode};
pub use settings::{
    DEFAULT_REASON_MIN_CHARS, POLICY_KEY, REASON_MIN_KEY, SettingsPolicy, policy_registry,
    setting_specs,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-governance";
