//! `spot-migration-approval` — review and approval of a release plan (blueprint WS12,
//! WP-MIG-04, ADR-22).
//!
//! The plan engine (`spot-migration-plan`) says what an upgrade will do. This crate makes sure
//! apply runs **exactly what a human approved and nothing else**:
//!
//! - [`PlanDigest`] is SHA-256 over a domain prefix and the plan's canonical text, so any change
//!   to any step, risk class or estimate input is a different digest.
//! - [`ApprovalPolicy`] (kept in settings under [`SETTING_KEY`]) says, per risk class, who must
//!   approve: nobody for additive plans if the site allows it, `n` distinct people in a role
//!   for plans that need review, two people and optionally a hardware signature for
//!   destructive ones. Approvals expire.
//! - [`Approval`] records and an [`ApprovalStore`]; the latest decision per approver counts, and
//!   an approval for another digest never counts.
//! - [`authorise_apply`] is the **only** way to an [`ApplyTicket`], which the apply engine
//!   (WP-MIG-07) requires. A modified, unapproved, rejected or stale-approved plan is refused.
//! - [`ApprovalService`] records and audits every action as a `spot-audit-chain` record, and
//!   [`cli`] gives the `plan show`, `approve`, `reject` and `status` subcommands.
//!
//! - `adapters` (features `sod`, `signed-command`, `settings`, `audit-chain`, default off)
//!   are the real implementations of the traits above.
//!
//! The crate reads no clock (time is an argument), has no float arithmetic and no `unsafe`.

#[cfg(any(
    feature = "sod",
    feature = "signed-command",
    feature = "settings",
    feature = "audit-chain"
))]
pub mod adapters;
pub mod approval;
pub mod authorise;
pub mod cli;
pub mod digest;
pub mod error;
pub mod policy;
pub mod service;

pub use approval::{
    Approval, ApprovalStore, AuthorshipCheck, Checks, Decision, Identity, InMemoryApprovalStore,
    NoAuthors, SignatureHook,
};
pub use authorise::{ApplyTicket, Evaluation, authorise_apply, effective_risk, evaluate};
pub use digest::{DOMAIN_PREFIX, PlanDigest};
pub use error::{ApprovalError, Refusal, codes};
pub use policy::{
    AdditiveRule, ApprovalPolicy, DestructiveRule, Requirement, ReviewRule, SETTING_KEY,
};
pub use service::{
    Act, ApprovalService, AuditSink, AuthoriseError, MemoryAuditSink, Status, risk_name,
};
