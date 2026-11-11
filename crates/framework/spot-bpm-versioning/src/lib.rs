//! `spot-bpm-versioning` — process versioning, optional Case migration and cancellation
//! compensation (WS8, WP-BPM-09.T1; ADR-49, ADR-071, ADR-080).
//!
//! # Running Cases stay pinned
//!
//! A Case records the process version and the compiled content hash it started on. Publishing a
//! new version through [`version::publish`] adds it to the engine's registry and never writes a
//! Case: [`version::publish`] returns the pins every running Case kept, and
//! [`version::pinned`] proves the registry still holds the exact definition a Case pins.
//!
//! # Optional migration
//!
//! [`migration::plan_migration`] is a pure dry run: it maps only the nodes the Case can still
//! run, preserves completed-node history, checks the target's variable contract and refuses an
//! unmapped current node with `bpm_versioning.unmapped_node`. [`migration::MigrationPlan::apply`]
//! stores the migrated record and its audit event through one port
//! ([`migration::MigrationTarget`]), in one transaction.
//!
//! # Bulk migration
//!
//! [`batch`] is an ADR-071 run: a stable identity, a status machine, per-item outcomes, a
//! resumable checkpoint and the "start twice is refused, resume instead" rules. The driver
//! ([`batch::BulkMigrator`]) applies the precomputed plans through the same migration port.
//!
//! # Cancellation and compensation
//!
//! [`compensation::CancellationWatcher`] consumes committed `doc_cancelled` events, finds the
//! Cases whose Case file contains the document, and calls a [`compensation::Compensator`]. The
//! default [`compensation::DeclaredCompensator`] raises the declared return step, or parks the
//! Case at `needs_decision` with its dependencies. A blocked Case may not take another engine
//! step until the step completes or a person releases the decision.
//!
//! # Purity
//!
//! Time is always an argument, no wall clock is read, there is no `unsafe`, no float arithmetic
//! and no `unwrap`/`expect` outside tests. The crate never deletes or edits a ledger-like row:
//! compensation only starts lifecycle work.

#![forbid(unsafe_code)]

pub mod batch;
pub mod compensation;
pub mod error;
pub mod mapping;
pub mod migration;
pub mod version;

pub use batch::{
    BatchCounters, BatchItem, BatchLedger, BatchProgress, BatchRequest, BatchRun, BatchStatus,
    BulkMigrator, ItemOutcome, ItemState, MIGRATION_BATCH_KIND,
};
pub use compensation::{
    CancellationReport, CancellationWatcher, CaseIndex, CompensationJournal, CompensationModel,
    CompensationOutcome, CompensationPath, CompensationRequest, CompensationSink, Compensator,
    DOCUMENT_CANCELLED_TOPIC, DeclaredCompensator, MemoryCaseIndex, MemoryCompensationSink,
    NEEDS_DECISION_TASK, ParkedCase, RaisedStep, TriggerRecord,
};
pub use error::{RefusedNode, Result, VersioningError};
pub use mapping::MigrationMapping;
pub use migration::{
    AppliedMigration, DryRunReport, MIGRATION_AUDIT_TOPIC, MemoryMigrationTarget, MigrationAudit,
    MigrationPlan, MigrationTarget, NodeChange, VariableAction, VariableChange, plan_migration,
};
#[allow(deprecated)]
pub use version::publish;
pub use version::{PinnedCase, PublishCheck, PublishReport, VersionRef, pinned, publish_guarded};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-versioning";
