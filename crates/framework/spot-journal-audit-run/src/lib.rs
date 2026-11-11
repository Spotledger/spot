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
//! * A rebuild that fails is [`AuditRun::Failed`] with the failing crate's code, not an error:
//!   the schedule wants a result row either way.
#![forbid(unsafe_code)]

mod error;
pub mod memory;
mod run;
mod sides;

#[cfg(test)]
mod tests;

pub use error::AuditRunError;
pub use run::{
    AuditOptions, AuditReport, AuditRun, FailureStage, RunFailure, SnapshotMarking, run_audit,
};
pub use sides::{Clock, LiveSide, ScratchSide, Summaries};
