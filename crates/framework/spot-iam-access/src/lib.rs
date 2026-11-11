//! The rules that join the identity crates (WP-IAM-07.T1, ADR-070 sections 5 and 9): API-key
//! scope, keys that stop with their owner, impersonation, step-up and the method ladder, as a
//! pure core that the login service and the engine's principal source call.
//!
//! # Shape
//!
//! - [`scope`]: [`KeyScope`] (what a key may do, as data) with
//!   [`check_creation`](KeyScope::check_creation) (every grant is one the owner holds now,
//!   all excesses listed) and [`decide`](KeyScope::decide) (the owner's decision AND the key's
//!   allow-list AND the key's value restriction).
//! - [`standing`]: [`standing`](standing::standing) of a key on **every use**, never cached,
//!   and the [`AuthOutcome`] the login service returns.
//! - [`impersonation`]: [`Impersonation::begin`] and [`Impersonated`]: both identities in
//!   the trail, administrator and break-glass roles stripped, expiry with the grant.
//! - [`stepup`]: [`StepUpPolicy`] (a default list a tenant may extend but never shrink) and
//!   [`MethodGuard`] with the [`AuthMethod`] ladder.
//! - [`error`]: the stable `iamaccess.*` codes.
//!
//! # Rules
//!
//! - **A key is a proper subset of its owner, forever.** The key's decision is the owner's
//!   decision at the time of the request, narrowed by the key; nothing the key says can add a
//!   right. When the owner loses a role the key loses the right without being edited. Field
//!   rights are the owner's: a key adds no field right, so callers answer field questions
//!   with the owner's principal and the key's document decision.
//! - **A key stops with its owner.** Standing is read from the owner's state on every use.
//! - **Impersonation is never an escalation.** The effective principal is the target's own
//!   assignments minus forbidden roles (and any role inheriting one); the grant adds nothing.
//! - **Time is an argument** (unix seconds); there is no clock, storage or network here.
//!
//! # Purity
//!
//! No storage, no clock, no ambient reads, no floats, no `unsafe`, no `unwrap`/`expect`
//! outside tests (the project coding rules). Errors never say whether a key id, an owner or a
//! target exists.

#![forbid(unsafe_code)]

pub mod error;
pub mod impersonation;
pub mod scope;
pub mod standing;
pub mod stepup;

pub use error::codes;
pub use impersonation::{
    Identity, Impersonated, Impersonation, ImpersonationError, ImpersonationPolicy,
    ImpersonationRefusal, ImpersonationTarget, ImpersonationTrail, PrincipalClass, StripReason,
    Stripped, SupportPrincipal,
};
pub use scope::{AllowRule, Excess, KeyAction, KeyScope, ScopeError, ScopeErrors};
pub use standing::{AuthOutcome, KeyRecord, OwnerState, Standing, check_new_key, standing};
pub use stepup::{
    AuthMethod, DEFAULT_STEP_UP_OPERATIONS, MethodError, MethodGuard, OperationName, StepUpPolicy,
    StepUpRequired, TargetSignIn,
};
