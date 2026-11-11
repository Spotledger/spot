//! Support-session grants for the control plane (WP-CTL-07.T1, blueprint WS18 and WS13).
//!
//! A support agent reaches a customer's tenant only through a **grant**: customer-approved,
//! scoped to named DocTypes and actions, capped at a tier, running over a signed window, and
//! revocable at any moment. This crate adds the control-plane workflow and the customer's own
//! record on top of the pieces that already exist:
//!
//! - [`spot_support_session`] is the scoped, time-boxed session model and the one action
//!   check (its fixed refusal order is preserved here).
//! - [`spot_agent_tiers`] is the agent registry: a grant to an agent must match its
//!   registered domain (support) and maximum tier.
//! - [`spot_audit_chain`] is the tenant's append-only, hash-chained audit log, where every
//!   request, approval, session start and stop, action and revocation is recorded — the
//!   customer's copy, not only the vendor's.
//!
//! # Shape
//!
//! - [`model`]: [`Grant`] (the brief's grant record), [`GrantSpec`], [`PendingGrant`] and
//!   the scope summary.
//! - [`policy`]: [`GrantPolicy`], the configured duration cap and condition budget. A grant
//!   always has an end instant; no "permanent" grant exists in the type system.
//! - [`customer`]: [`CustomerAdmins`], the only people who may approve or extend a grant.
//! - [`store`]: [`GrantStore`] and the reference [`MemoryGrantStore`]; there is no delete.
//! - [`enforce`]: [`check`], the pure `Allow | Deny(reason)` decision over the session model.
//! - [`record`]: [`GrantEvent`], [`CustomerLog`], [`AuditExecutor`] and [`ChainLog`], the
//!   tenant's chain as an append-only writer.
//! - [`plane`]: [`ControlPlane`], the workflow: request, approve, start, act, stop, revoke
//!   and extend, with every step recorded on the customer side.
//! - [`view`]: [`CustomerView::sessions`] — who, what scope, what was done, what changed,
//!   and whether the tenant's chain still verifies.
//!
//! # Rules this crate is
//!
//! - **A grant exists only after a customer administrator approves it.** The approver must
//!   be registered for the tenant and must differ from the requester and the subject.
//! - **Scope, tier and expiry are enforced on every action** by the session model's check;
//!   an out-of-scope or expired action is denied *and* recorded on the customer side.
//! - **Revocation is immediate and final.** It works before, during or after a session, and
//!   the grant stays as evidence.
//! - **Extensions are new approvals**, by a customer administrator, within the same total
//!   duration cap, and they stay in the grant's history.
//! - **The customer's record cannot be suppressed.** The chain writer can only append, and
//!   the view reports tampering as `chain_verified: false`; if an allowed action cannot be
//!   recorded, it is refused.
//!
//! # Purity
//!
//! No clock reads (time is an argument), no randomness, no network, no floats, no `unsafe`
//! and no `unwrap`/`expect` outside tests (the project coding rules).

#![forbid(unsafe_code)]

pub mod customer;
pub mod enforce;
pub mod error;
pub mod model;
pub mod plane;
pub mod policy;
pub mod record;
pub mod store;
pub mod view;

pub use customer::CustomerAdmins;
pub use enforce::{Action, Decision, Denial, SessionRef, check};
pub use error::{GrantError, codes};
pub use model::{Grant, GrantSpec, PendingGrant, scope_summary};
pub use plane::ControlPlane;
pub use policy::GrantPolicy;
pub use record::{AuditExecutor, ChainLog, CustomerLog, EventKind, GrantEvent};
pub use store::{GrantStore, MemoryGrantStore};
pub use view::{ActionRecord, CustomerView, ExtensionRecord, SessionRecord};

/// The name of this crate, for diagnostics and record headers.
pub const CRATE_NAME: &str = "spot-support-grants";
