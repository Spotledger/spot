//! `spot-bpm-effects`: the request record file and the undo machinery (WS8, WP-BPM-15.T1;
//! ADR-080 sections 6.4 to 6.6 and 6.9).
//!
//! A request (a Case of the process engine) changes records through the record types' own
//! lifecycles, and every operation returns a receipt of what it caused. This crate keeps those
//! receipts, in order, in the request's **record file** ([`RecordFile`]), and answers two
//! different questions about going back:
//!
//! - **Rewind** ([`RequestEffects::rewind`]) moves the request pointer back and keeps every
//!   effect. The next pass revises the records it created, through `update` or `correct`
//!   ([`RequestEffects::revise_of`]). It refuses when the revised path would create a record a
//!   second time and the process declares no revision for that node (rule 6.8).
//! - **Undo** ([`RequestEffects::plan_undo`], [`RequestEffects::execute_undo`]) reverses the
//!   effects after a node, **through the lifecycle, as the acting principal**, in reverse
//!   dependency order: a record before the records it depends on is reversed after those, ties
//!   by reverse time. Each entry carries the reversals its declarations name (`ReversibleBy`:
//!   a lifecycle operation; `CompensableBy`: a named action; `Irreversible`: a point of no
//!   return that blocks the whole plan before anything runs).
//!
//! # Refusals are decisions, not failures
//!
//! When the lifecycle refuses a reversal (dependants outside the request, a closed period, a
//! type rule) the sequence stops in [`UndoOutcome::NeedsDecision`] with the [`Blocker`]s and the
//! [`Alternative`]s the record type declares. Nothing is forced. A person picks one alternative
//! or abandons ([`RequestEffects::choose`]); the pick is audited.
//!
//! # Never deletes, never writes a store
//!
//! This crate has no storage dependency. A reversal is a call on the [`ReversalPort`], whose
//! lifecycle appends reversing rows; the file only grows (a reversal is a new entry that points
//! at the entry it reverses), and the request lock refuses a forward step, a rewind or a second
//! undo while an undo holds the request.
//!
//! # Idempotent and resumable
//!
//! The journal, keyed by `(entry, reversal)`, lives in the file; the port is given the request
//! key `undo:<request>:<entry>:<reversal>` so a resumed undo never reverses twice.
//!
//! # No secrets, no peer text
//!
//! Entries hold receipts (references, counts, digests), never field values. A refusal code that
//! is not a stable code shape is replaced; alternatives are tokens, not text.
//!
//! Time is always an argument; there is no wall clock here.

#![forbid(unsafe_code)]

pub mod declarations;
pub mod error;
pub mod graph;
pub mod model;
pub mod port;
pub mod service;
pub mod undo;

pub use declarations::{Declarations, StaticDeclarations, derive_reversals};
pub use error::{EffectsError, Result, codes};
pub use graph::{GraphDependants, NoGraph};
pub use model::{
    AuditRecord, EntryReversal, FileEntry, JournalKey, NewEntry, PartnerWait, RecordFile,
    RecordRef, Reverses, ReviseHow, Revision, UndoRun,
};
pub use port::{
    Alternative, AlternativeId, AlternativeKind, CancellationCall, PartnerPort, PortError, Refusal,
    ReversalCall, ReversalPort, Via,
};
pub use service::{RequestEffects, RewindOutcome, RewindPath, UndoContext};
pub use undo::{
    Blocker, CancellationStep, Choice, ChoiceOutcome, Decision, PlannedReversal, SkipReason,
    UndoOutcome, UndoPlan, UndoStep,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-effects";
