//! The journal durability policies (WP-JRN-04.T1, ADR-061 s4).
//!
//! A pure library: an input-driven state machine ([`AckGate`]) that decides when a commit may be
//! acknowledged under each policy, and what happens when the object store cannot be reached. It
//! touches no store, network, clock or random source; time is an argument of every call. The
//! relay and the control-plane setting call this crate and do not re-implement its rules.
//!
//! | Policy | A commit is acknowledged when |
//! | --- | --- |
//! | [`Policy::Local`] | the local database commit is durable |
//! | [`Policy::ReplicaSync`] | the synchronous standby has confirmed the commit |
//! | [`Policy::RemoteSync`] | a tail chunk covering it is durable in the object store (group commit) |
//!
//! Under `remote-sync` the store being unreachable is handled by [`OnUnreachable`]: `degrade`
//! acknowledges on the local commit, remembers the backlog and raises an [`RpoBreach`] after a
//! grace; `halt` refuses new commits and resumes by itself when the store returns.

mod error;
mod gate;
mod policy;

pub use error::DurabilityError;
pub use gate::{
    AckBasis, AckGate, Admission, CommandOutcome, Event, FailureReason, GateConfig, Output,
    Refusal, RpoBreach,
};
pub use policy::{OnUnreachable, Policy, StandingWarning, TenantDurability};

/// A sequence number within one generation of a source's feed.
pub type Seq = u64;

/// The identifier of one group-commit batch (one object-store round trip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BatchId(pub u64);
