//! `spot-fix-pipeline` — the fix pipeline for agent actions (blueprint WS13, WP-AI-08.T1,
//! ADR-26).
//!
//! Every change an agent makes runs **plan → rehearse → apply → verify**, with an automatic
//! rollback and a change journal. This is the hard floor of the self-healing support workstream:
//! a change is journaled and snapshotted first, the audit log cannot be disabled, and a failed
//! verification leaves the site **byte-identical to its pre-state by fingerprint** — or the fix
//! is [`FixState::Failed`], never a false claim that it rolled back.
//!
//! # Shape
//!
//! - [`model`]: [`FixId`], [`FixState`], [`FixPlan`] (the normalised promise with its
//!   [`FixHash`]), [`FixSource`], [`Risk`], [`Mechanism`], [`PreImage`], [`ChangeRecord`],
//!   [`VerifyReport`], [`ResidueEvidence`], [`UndoWindow`] and [`FixOutcome`].
//! - [`policy`]: [`FixPolicy`] and [`PlanPath`] — the rules that decide direct apply versus
//!   rehearse, pre-images versus a snapshot, and whether a T3 plan needs a human.
//! - [`journal`]: the write-ahead [`FixJournal`] trait, the in-memory [`InMemoryJournal`], the
//!   [`JournalRecord`] vocabulary and the [`Progress`] fold a restart reads.
//! - [`audit`]: [`FixAuditEvent`] and [`MemoryFixAudit`]; each transition maps onto a valid
//!   `spot-audit-chain` record carrying the agent identity, tier and deployment epoch.
//! - [`ports`]: the typed ports — [`PreImageStore`], [`SnapshotPort`], [`FixExecutor`],
//!   [`RehearsalPort`], [`FixVerifier`], [`ApprovalPort`] and [`FixAudit`].
//! - [`engine`]: [`FixEngine`] with `run`, `recover`, `undo` and `close_expired`.
//! - [`faults`]: deterministic kill points for crash tests.
//! - [`testing`] (feature `testing`): in-memory doubles for the ports.
//! - [`error`]: [`FixError`] with its stable `fix_pipeline.*` codes.
//!
//! # The rules this crate enforces
//!
//! - **The journal and the pre-image cannot be bypassed.** The only apply path journals the
//!   intent, takes a snapshot for a T3/large plan or captures a pre-image for every affected
//!   target, and journals the live pre-state fingerprint before the first step.
//! - **A failed verification rolls back automatically.** The inverse runs, the invariants run
//!   again, and the residue check (`pre.diff(post_rollback)` over the fingerprint engine of
//!   `spot-rehearsal`) must be empty.
//! - **T3 always rehearses and needs approval.** A registered deep agent may self-approve per
//!   policy; everyone else goes through the BPM task inbox across [`ApprovalPort`].
//! - **A verified fix stays undoable for a window.** Undo detects later changes before it runs.
//! - **Time is an argument, never a clock**; no float arithmetic for money, quantities or time;
//!   no `unsafe`; no `unwrap`/`expect` outside tests.
//!
//! # Example
//!
//! ```
//! use spot_fix_pipeline::{DeploymentEpoch, FixId, FixState, UndoWindow};
//!
//! assert_eq!(FixState::Applied.as_str(), "applied");
//! assert!(FixState::Verified.is_terminal());
//! assert!(!FixState::Applied.is_terminal());
//! assert!(UndoWindow::open(100, 60).is_open(120));
//! assert!(DeploymentEpoch::new(7) == DeploymentEpoch::new(7));
//! assert!(FixId::new("fix-abc-1").is_ok());
//! ```

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod audit;
pub mod engine;
pub mod error;
pub mod evidence;
pub mod faults;
pub mod journal;
pub mod model;
pub mod policy;
pub mod ports;
#[cfg(feature = "testing")]
pub mod testing;

pub use audit::{AuditAction, FixAuditEvent, MemoryFixAudit};
pub use engine::{FixContext, FixEngine, FixPorts};
pub use error::{FixError, Result, codes};
pub use faults::{Faults, KillAt, KillPoint, NoFaults};
pub use journal::{FixJournal, InMemoryJournal, JournalEntry, JournalRecord, Phase, Progress, Seq};
pub use model::{
    AgentRef, ChangeRecord, DeploymentEpoch, FailureEvidence, FailureStage, FixHash, FixId,
    FixOutcome, FixPlan, FixSource, FixState, FixStep, Mechanism, PreImage, RehearsalEvidence,
    ResidueEvidence, Risk, RollbackCause, RollbackEvidence, SnapshotRef, StructuralKind,
    UndoWindow, VerifyReport,
};
pub use policy::{FixPolicy, PlanPath};
pub use ports::{
    ApprovalPort, FixAudit, FixExecutor, FixVerifier, PreImageStore, RehearsalPort, SnapshotPort,
    StepChange,
};

/// The name of this crate, for diagnostics and record headers.
pub const CRATE_NAME: &str = "spot-fix-pipeline";
