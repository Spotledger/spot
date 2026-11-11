//! `spot-rescue` — down-patches and the supervised restore-and-re-entry rescue tool
//! (blueprint WS12, WP-MIG-10, ADR-22).
//!
//! The last rollback window of ADR-22 is the one where users have already worked on the new
//! version. There is no clean rollback any more: the old schema no longer holds the documents
//! created since the migration. This crate makes that window supervised rather than lost:
//!
//! 1. **Down-patches** ([`DownPlan`], [`DownPatcher`]) run the declared `down` step of every
//!    applied reversible patch in reverse apply order, through exactly the same
//!    [`StepRunner`](spot_migration_apply::StepRunner) trait as the apply engine. An
//!    irreversible patch stops the plan before anything runs, with [`Error::Irreversible`]
//!    naming every patch that has no way back.
//! 2. **The re-entry planner** ([`ReentryPlanner`]) reads the operations committed after the
//!    restored snapshot at time `T`, orders them causally, and classifies each one as
//!    [`Classification::Replayable`], [`Classification::NeedsTransform`] (a declared
//!    [`DownTransform`] maps a changed field back to its old shape) or
//!    [`Classification::Blocked`] (the old version cannot express it).
//! 3. **Supervised execution** ([`RescueEngine`]) restores the snapshot, then re-enters every
//!    item as an ordinary operation through the old version's [`Reenter`] engine, so its
//!    validations and postings run. Batches carry ADR-071 run identity, checkpoints and
//!    per-item outcomes; a failed item stops the run and is reported with its original data
//!    intact. The tool never discards anything and never deletes the post-migration database.
//! 4. **Safety** ([`RestoreApprovals`], [`Invariants`]): a two-person `restore` ticket is
//!    required, the maintenance gate is closed before the first write and opened only after
//!    the final invariant pass, and the report lists counts per outcome and every item that
//!    needs a person.
//!
//! # Restart
//!
//! A kill (the process dying) leaves the journal and the item outcomes behind. A new engine
//! over the same [`RescueJournal`] resumes: a completed restore is not repeated, a `done` item
//! is not presented to the old engine again, and the checkpoint only saves re-scanning because
//! the outcomes are the truth (ADR-071 section 3).
//!
//! # What this crate does not do
//!
//! It never connects to a database, reads a clock, reads the environment or writes SQL. The
//! repository, the outbox/history reader, the old-version engine, the snapshot provider and the
//! approval API are all seams. Time is an argument, money and quantities are handled by the
//! old engine (this crate does no arithmetic on them), and there is no `unsafe`.
//!
//! # Example
//!
//! ```
//! use spot_rescue::{DownPlan, PatchDecl};
//!
//! let decl = PatchDecl::parse(
//!     "id = \"acme_0001\"\nintent = \"add a field\"\nclass = \"additive\"\n\
//!      reversible = true\nidempotent = true\n[down]\nid = \"acme_0000\"\n",
//! )
//! .expect("a valid declaration");
//! let plan = DownPlan::build("plan-digest", vec![decl]).expect("reversible");
//! assert_eq!(plan.steps()[0].down_id, "acme_0000");
//! ```

pub mod codes;
pub mod down;
pub mod engine;
pub mod error;
pub mod faults;
pub mod invariants;
pub mod journal;
pub mod reentry;
pub mod ticket;

pub use down::{
    DownOutcome, DownPatcher, DownPlan, DownReport, DownRequest, DownStep, DownStepReport,
};
pub use engine::{
    ItemReport, NeedsPerson, OutcomeCounts, Reenter, ReenterError, ReenterOutcome, RescueEngine,
    RescueOutcome, RescueReport, RescueRequest, RescueStatus,
};
pub use error::{Error, Result};
pub use faults::{Faults, KillAt, KillPoint, NoFaults};
pub use invariants::{
    InvariantCheck, InvariantPhase, InvariantReport, Invariants, RecordingInvariants,
};
pub use journal::{
    InMemoryRescueJournal, ItemKey, ItemOutcome, JournalEntry, JournalRecord, RescueJournal,
    RunIdentity, RunProgress, Seq,
};
pub use reentry::{
    Classification, CombinedOperations, CommittedOperation, DownTransform, FixedOperations,
    OldDocType, OldSchema, OperationId, OperationKey, OperationKind, OperationSource, PlanCounts,
    PlannedOperation, ReentryItem, ReentryPlan, ReentryPlanner,
};
pub use spot_release_meta::PatchDecl;
pub use ticket::{MemoryRestoreApprovals, RestoreApprovals, RestoreTicket};

/// A boxed future: what the object-safe async seams return.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The operation payload representation the rescue reads and transforms.
///
/// It is the ADR-055 value encoding as JSON; the old engine is responsible for the typed
/// conversion and for money/quantity arithmetic.
pub type Payload = serde_json::Value;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-rescue";
