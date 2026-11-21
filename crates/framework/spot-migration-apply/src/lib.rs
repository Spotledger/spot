//! `spot-migration-apply` — the apply engine of the migration lifecycle (blueprint WS12,
//! WP-MIG-07.T1, ADR-22).
//!
//! A migration runs **plan → approve → rehearse → apply → verify**. This crate is the *apply*
//! step: it takes an approved plan and a taken snapshot, closes the site to users, holds external
//! side effects, runs the plan's ordered steps with a write-ahead journal, verifies the result,
//! and admits users only after verification passed or the snapshot was restored.
//!
//! # The state machine
//!
//! [`ApplyState`] is `Idle -> Gated -> Applying(step n) -> Verifying -> Open`, with the failure
//! edges `-> Restoring -> Open(old)` and `-> Halted(needs person)`. Every transition is written
//! to the [`ApplyJournal`] **before** it happens ([`ApplyEngine::apply`]).
//!
//! # A restart
//!
//! [`ApplyEngine::recover`] reads the journal and either resumes at the first incomplete step
//! (when the steps make that safe) or restores per the rollback table of ADR-22: a transactional
//! step's transaction aborted with the crash, a non-transactional step is undone from its
//! recorded compensating restore point, and a run that reached verification is restored from the
//! snapshot. The gate is re-closed first, so **a crash never opens the site**.
//!
//! # Failing closed
//!
//! The engine opens the maintenance gate on exactly two paths: a verified new version and a
//! restored old version. Every error path leaves the gate closed. A step whose idempotency is
//! undeclared or declared at-most-once is never run twice: a restart restores instead of
//! resuming it.
//!
//! # What this crate does not do
//!
//! It never connects to a database, reads a clock or the environment, or writes SQL: the steps
//! are run through [`StepRunner`], the snapshot through [`SnapshotPort`], the hold through
//! [`SideEffectHold`], and time is an argument. The PostgreSQL wiring is T2. There is no `unsafe`
//! and no float arithmetic.
//!
//! # Example
//!
//! ```
//! use spot_migration_apply::{ApplyOptions, ApplyState, KillPoint, Version};
//!
//! assert_eq!(ApplyOptions::default().drain_deadline_micros, 30_000_000);
//! assert_eq!(KillPoint::BeforeVerify.label(), "before_verify");
//! assert!(ApplyState::Idle.is_finished());
//! ```

pub mod codes;
pub mod engine;
pub mod error;
pub mod faults;
pub mod gate;
pub mod hold;
pub mod journal;
pub mod progress;
pub mod snapshot;
pub mod state;
pub mod step;
pub mod verify;

#[cfg(feature = "testing")]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub mod testing;

pub use engine::{ApplyEngine, ApplyOptions, ApplyOutcome, ApplyRequest, ApplyService};
pub use error::{Error, Result};
pub use faults::{Faults, KillAt, KillPoint, NoFaults};
pub use gate::{
    AdmissionGate, DrainReport, GateStatus, InMemoryAdmissionGate, MAINTENANCE_CODE,
    MaintenanceProblem,
};
pub use hold::{HoldServiceBridge, RecordingHold, SideEffectHold};
pub use journal::{
    ApplyJournal, InMemoryJournal, JournalEntry, JournalRecord, Progress, Seq, StepStart,
};
pub use progress::{ProgressEvent, ProgressSink, RecordingProgress, SilentProgress};
pub use snapshot::{RecordingSnapshotPort, RestoreOutcome, SnapshotId, SnapshotPort};
pub use state::{
    ApplyState, RecoveryAction, RecoveryOutcome, RestoreCause, RestoreMechanism, Version,
};
pub use step::{
    Checkpoint, Idempotency, RestorePoint, StepContext, StepDescriptor, StepOutcome, StepRunner,
};
pub use verify::{
    AppliedState, InvariantResult, InvariantSet, PlanVerifier, StaticInvariantSet, VerifyOutcome,
    VerifyPort, Violation,
};

#[cfg(feature = "testing")]
pub use testing::{
    FakeApplied, FakeRunner, FakeSite, FakeSnapshotPort, FakeVerify, StepConfig, sample_plan,
    site_id, ticket_for,
};

/// A boxed future: what an object-safe async method returns.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
