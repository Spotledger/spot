//! The remedy catalogue framework (blueprint WS13, WP-AI-07.T1, ADR-26).
//!
//! T2 repair of data is by remedies from this catalogue only; an agent cannot improvise a data
//! change. A **remedy** is a typed, tested operation with a precondition, a mandatory dry-run, an
//! undo and its own self-test. Every change goes through the typed ports on
//! [`RemedyCtx`](ctx::RemedyCtx) — the document lifecycle, jobs, the cache, a retrieval index,
//! edge sync, quarantine and BPM cases — never a raw storage handle.
//!
//! # Shape
//!
//! - [`model`]: [`RemedyId`](model::RemedyId), [`RemedyMeta`](model::RemedyMeta) (tier, risk,
//!   affected document types, a JSON Schema parameter subset, the precondition text and the undo
//!   kind), [`Check`](model::Check), the sealed [`Plan`](model::Plan), [`Change`](model::Change)
//!   and the [`UndoRecord`](model::UndoRecord).
//! - [`ports`]: the typed operations, the only way a remedy touches the world.
//! - [`ctx`]: [`RemedyCtx`](ctx::RemedyCtx), the [`Gate`](ctx::Gate) and the hard floor's
//!   enforcement ([`RemedyCtx::apply_document`](ctx::RemedyCtx::apply_document)).
//! - [`remedy`]: the object-safe [`Remedy`](remedy::Remedy) trait.
//! - [`registry`]: [`Registry`](registry::Registry), tier gating through
//!   [`spot_agent_tiers::Authoriser`], the mandatory dry-run and the undo log.
//! - [`seeds`]: the first generic remedies (`job.retry_failed`, `job.unstick_running`,
//!   `cache.flush_namespace`, `index.rebuild_retrieval`, `edge.repair_drift`,
//!   `quarantine.retry_all`, `case.unstick`).
//! - [`scenario`] and [`testing`]: the self-test harness (`remedy_test::run`) and the in-memory
//!   [`fixture`].
//! - [`corpus`]: the support-case corpus format, its loader/validator and the deterministic
//!   [`suggest`](corpus::Corpus::suggest) matcher.
//! - [`audit`]: the audit trail, where every decision and every undo is recorded.
//! - [`error`]: [`RemedyError`](error::RemedyError) with its stable `remedies.*` codes.
//!
//! # Rules
//!
//! - A remedy runs only through the [`Registry`](registry::Registry), only when the tier
//!   authoriser allows its action for the agent under the hard floor.
//! - **Dry-run is mandatory**: apply takes the [`Plan`](model::Plan) that dry-run produced, and a
//!   plan hash binds the two.
//! - **Undo is audited** and refuses an unknown or expired token, a later conflict, or an
//!   irreversible remedy — which must be T3.
//! - **The hard floor binds every remedy**: a submitted financial record changes only through
//!   cancel, amend or a journal entry.
//! - Time is an argument; there is no clock, no float arithmetic, no `unsafe`, and no
//!   `unwrap`/`expect` outside tests.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod audit;
pub mod corpus;
pub mod ctx;
pub mod error;
pub mod fixture;
pub mod model;
pub mod ports;
pub mod registry;
pub mod remedy;
pub mod scenario;
pub mod seeds;
mod seeds_more;
pub mod testing;

pub use audit::{AuditEvent, AuditKind, AuditSink, MemoryAudit};
pub use corpus::{Corpus, CorpusCase, Outcome, Suggestion};
pub use ctx::{Gate, RemedyCtx, RemedySettings};
pub use error::RemedyError;
pub use fixture::FakeWorld;
pub use model::{
    Applied, Change, Check, DocTypeRef, ParamSchema, ParamType, Plan, PlanHash, PlanStep, RemedyId,
    RemedyMeta, Risk, UndoKind, UndoRecord, UndoToken, Undone,
};
pub use ports::{
    CacheChange, CacheNs, CacheOps, CaseChange, CaseLife, CaseOps, CaseRef, CaseView, DocOp,
    DocRef, DocState, DocumentChange, DocumentOps, DocumentView, EdgeChange, EdgeOps, EdgeRef,
    EdgeView, IndexChange, IndexHealth, IndexOps, IndexRef, IndexView, JobChange, JobLife, JobOps,
    JobRef, JobView, OpError, QuarantineChange, QuarantineItem, QuarantineOps, QuarantineRef,
    QuarantineState,
};
pub use registry::Registry;
pub use remedy::Remedy;
pub use scenario::{Scenario, TestSpec, UndoExpectation, WorldSpec};

/// The self-test harness, re-exported as `remedy_test`.
pub use testing as remedy_test;

/// The name of this crate, for diagnostics and record headers.
pub const CRATE_NAME: &str = "spot-remedies";
