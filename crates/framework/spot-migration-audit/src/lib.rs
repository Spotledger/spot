//! `spot-migration-audit` — the migration audit record (blueprint WS12, WP-MIG-11, ADR-22).
//!
//! A migration runs **plan → approve → rehearse → apply → verify**. This crate is the record of
//! every run, kept in the tamper-evident audit chain so nobody can alter it afterwards, plus the
//! query API a console lists and explains runs through, and the guard that a migration runs only
//! through the runtime with a role permission.
//!
//! - [`MigrationRun`] is the record: the plan (by reference and digest), its approvals, the
//!   rehearsal report, the apply journal, the outcome, the timings and the actor. Large
//!   artefacts are stored **by reference** ([`ArtifactRef`]) with their SHA-256; a plan secret
//!   or a connection string never enters the record.
//! - [`RunRecorder`] is the trait the apply and rehearsal engines call; [`ChainRunRecorder`]
//!   appends each lifecycle event to the chain ([`kinds`]). [`RunRecorder::begin`] is the only
//!   source of a [`RunHandle`].
//! - [`RunStore`] is the query API: [`get`](RunStore::get) one run with its full evidence,
//!   [`list`](RunStore::list) by outcome, actor or date. [`ChainRunStore`] folds the chain back
//!   into runs. [`explain`] renders a run in plain language.
//! - [`permit`] checks the runtime permission [`RUN_PERMISSION`] (`migration.run`) and requires
//!   a [`RunHandle`], so an apply with no recorded run is refused. [`Permit`] and [`RunHandle`]
//!   have no public constructor.
//!
//! The crate reads no clock, no random source and no environment: time, ids and keys are always
//! arguments (ADR-077). It uses no float arithmetic, no `unsafe` and no `unwrap`/`expect` outside
//! tests.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use futures::executor::block_on;
//! use spot_audit_chain::{AuditChain, CheckpointPolicy, InMemoryStore, LocalSigner};
//! use spot_migration_audit::{
//!     ArtifactKind, ArtifactRef, ChainRunRecorder, ChainRunStore, Outcome, PlanRef, PlanSummary,
//!     RunFilter, RunId, RunRecorder, RunSpec, RunStore,
//! };
//!
//! let signer = Arc::new(LocalSigner::from_seed([7u8; 32]));
//! let chain = AuditChain::new(InMemoryStore::new(), "site-1", signer, CheckpointPolicy::default());
//! let recorder = ChainRunRecorder::new(chain);
//! let store = ChainRunStore::new(recorder.store(), "site-1");
//!
//! let spec = RunSpec {
//!     id: RunId::new("run-1").unwrap(),
//!     plan_digest: "abc".to_owned(),
//!     plan: PlanRef {
//!         artifact: ArtifactRef::of(ArtifactKind::Plan, "plans/run-1.json", b"plan"),
//!         summary: PlanSummary::default(),
//!     },
//!     actor: "user:alice".to_owned(),
//!     at: 1_000,
//! };
//! let _handle = block_on(recorder.begin(spec)).unwrap();
//!
//! let runs = block_on(store.list(RunFilter::all())).unwrap();
//! assert_eq!(runs.len(), 1);
//! assert_eq!(runs[0].outcome, Outcome::Planned);
//! ```
//!
//! # A permit cannot be forged
//!
//! [`Permit`] and [`RunHandle`] have private fields and no public constructor, so a caller
//! cannot build one directly:
//!
//! ```compile_fail
//! use spot_migration_audit::Permit;
//! let _forged = Permit { run: todo!(), site: todo!(), actor: todo!(), at: 0 };
//! ```
//!
//! ```compile_fail
//! use spot_migration_audit::RunHandle;
//! let _forged = RunHandle { id: todo!(), site: todo!(), actor: todo!() };
//! ```

pub mod error;
pub mod events;
pub mod explain;
pub mod permit;
pub mod recorder;
pub mod run;
pub mod store;

#[cfg(test)]
mod tests;

pub use error::{MigrationAuditError, codes};
pub use events::{
    ApplyEvent, ApprovalEvent, FinishEvent, RehearsalEvent, RunSpec, SUBJECT_DOCTYPE, kinds,
};
pub use explain::{Explanation, explain};
pub use permit::{Permit, RUN_PERMISSION, RoleAccess, RunAccess, permit};
pub use recorder::{ChainRunRecorder, RunHandle, RunRecorder};
pub use run::{
    ApprovalDecision, ApprovalRef, ArtifactKind, ArtifactRef, MAX_ID_BYTES, MigrationRun, Outcome,
    PlanRef, PlanSummary, RehearsalVerdict, ReportRef, RunEvidence, RunFilter, RunId, area_counts,
    digest_of, hex, risk_name,
};
pub use store::{ChainRunStore, RunStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-migration-audit";
