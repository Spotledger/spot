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
//!
//! # Positions carry the generation
//!
//! A source's feed is numbered per generation: after a fork or restore the next generation starts
//! again at `seq` 1 (ADR-061 amendment A.0, `docs/research/journal-position-numbering-across-forks.md`).
//! A bare sequence number therefore names one record only inside one generation, so every key,
//! input and output of the gate is a [`Position`] `(generation, seq)`, ordered generation first.
//! A commit of a **lower generation** than one already seen is a stale writer and is refused with
//! a typed error ([`DurabilityError::StaleGeneration`], reported as [`Output::Rejected`]); the gate
//! never silently drops a commit it does not accept.
//!
//! ## Migration for callers
//!
//! * `Seq` stays `u64` and now means the sequence number *within a generation*; the new
//!   [`Generation`] is `u64` too, and `Position::new(generation, seq)` joins them.
//! * Every `seq: Seq` field of [`Event`], [`Output`], [`CommandOutcome`], [`RpoBreach`] and
//!   [`DurabilityError`] became `position: Position` (`ChunkDurable::up_to_seq` became `up_to`,
//!   `RpoBreach::unshipped_from_seq` became `unshipped_from`). Build a position from the
//!   generation the relay is writing, which the caller already knows.
//! * `StandbyConfirmed` and `ChunkDurable` are cumulative over the whole position order: a
//!   confirmation of `2.1` also covers every position of generation 1.
//! * Match [`Output::Rejected`]; a commit that is out of order, a repeat or a stale generation is
//!   reported there instead of being ignored.

mod error;
mod gate;
mod policy;

pub use error::DurabilityError;
pub use gate::{
    AckBasis, AckGate, Admission, CommandOutcome, Event, FailureReason, GateConfig, Output,
    Refusal, RpoBreach,
};
pub use policy::{OnUnreachable, Policy, StandingWarning, TenantDurability};

use std::fmt;

/// A generation of a source's feed. A fork or restore opens the next one.
pub type Generation = u64;

/// A sequence number within one generation of a source's feed (it restarts at 1 in every
/// generation).
pub type Seq = u64;

/// A record's place in a source's feed: `(generation, seq)`. The derived order compares the
/// generation first, so every position of a later generation is greater than every position of an
/// earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    /// The generation.
    pub generation: Generation,
    /// The sequence number within the generation.
    pub seq: Seq,
}

impl Position {
    /// A position from its two parts.
    pub const fn new(generation: Generation, seq: Seq) -> Self {
        Self { generation, seq }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.generation, self.seq)
    }
}

/// The identifier of one group-commit batch (one object-store round trip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BatchId(pub u64);
