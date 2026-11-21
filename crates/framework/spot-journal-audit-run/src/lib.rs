//! The scheduled journal audit (WP-JRN-07.T2, ADR-061 s8).
//!
//! `spot-journal-audit` holds the comparison and the report model; `spot-journal-rebuild` holds
//! the executor. This crate joins them with the live database behind traits: [`run_audit`]
//! rebuilds the latest trusted snapshot plus the journal into a scratch side as of one recorded
//! position, compares the rebuilt records and fingerprints with the live side's as of the same
//! position, measures the rebuild against the tier target, and returns an [`AuditRun`].
//!
//! # Guarantees
//!
//! * **One tenant per run**, fixed in [`AuditOptions`]; a live or scratch side of another tenant
//!   is refused before anything runs.
//! * **The live side is read-only by trait**, and read as of the position recorded at the start,
//!   so writes that arrive during the run do not count.
//! * **The scratch side is always discarded**, also when the rebuild fails.
//! * **Digests only**: a result names the first differing record by entity, id and kind, and
//!   counts per entity. It never carries record content.
//! * **The run writes nothing but the snapshot marker** on the untrusted-snapshot path; the
//!   verified position of a clean run is returned for the caller to record.
//! * **The classifier is the caller's**: [`AuditOptions::classifier`] and the unregistered-topic
//!   policy are the rebuild's own options. The default is the events registry with the strict
//!   policy, so a journal that holds an unknown topic fails the rebuild stage with
//!   [`REBUILD_UNKNOWN_TOPIC`] instead of being audited by guess; a report says which classifier
//!   decided and lists the topics it did not know.
//! * A rebuild that fails is [`AuditRun::Failed`] with the failing crate's code, not an error:
//!   the schedule wants a result row either way.
//!
//! # Positions are pairs
//!
//! A position is `(generation, seq)` and every generation restarts at 1 (ADR-061 amendment A.0),
//! so a bare `seq` identifies nothing. The pinned position, the snapshot filter, the plan target
//! (`Target::In`) and the in-memory live side's ordering all use the pair, ordered
//! lexicographically. `VerifiedPosition` and `OldestSnapshot` of `spot-journal-audit` already
//! hold a `Position` with both fields, and their JSON keeps `generation` beside `seq`: a reader
//! of an older shape that knew only `seq` must read `generation` too.
#![forbid(unsafe_code)]

mod classify;
mod error;
pub mod memory;
mod run;
mod sides;
mod topics;

#[cfg(test)]
mod tests;

pub use classify::{ClassifierError, DomainTopicsClassifier};
pub use error::AuditRunError;
pub use run::{
    AuditOptions, AuditReport, AuditRun, ClassifierKind, FailureStage, RunFailure, SnapshotMarking,
    run_audit,
};
pub use sides::{Clock, LiveSide, ScratchSide, Summaries};
pub use spot_journal_rebuild::{EventClass, EventClassifier, RegistryClassifier};
pub use topics::{MAX_REPORTED_TOPICS, REBUILD_UNKNOWN_TOPIC, UnknownTopics, is_valid_topic};
