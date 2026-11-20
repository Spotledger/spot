//! The journal rebuild hosted (WP-RTB-01.T6, ADR-061 section 6).
//!
//! The planner (`spot-journal-rebuild-plan`) decides what a rebuild does and the executor
//! (`spot-journal-rebuild`) runs it into a sink; nothing rebuilt a real site database. This crate
//! is the host side of that, for a **stopped** site (an offline operation, like `spot migrate`):
//!
//! * [`rebuild_site`] ([`run`]): the order of a rebuild: tenant and running-site checks, the
//!   plan (full, roll-forward after a backup restore, point in time with a fork), `--dry-run`,
//!   the projector check before the first write, the executor into a checkpointing sink, the
//!   outbox adoption **before** the audit events, the projectors, the invariant suite, the report
//!   (counts and positions only);
//! * [`CheckpointingSink`] ([`sink`]) and [`CheckpointStore`] ([`checkpoint`]): a durable
//!   `(generation, seq)` checkpoint, resume without restarting or applying twice, a refusal to
//!   resume a changed plan, and the `Rebuilding` state that keeps a half-built database from
//!   serving;
//! * [`ReplicaSink`] ([`writer`]): the sink over the one replication apply path (no controllers,
//!   no scripts, no outbox events), with the panicking side-effects sentinel;
//! * [`Projectors`] ([`projector`]): derived entities are recomputed after the images, and a
//!   derived entity without a projector is a refusal found before anything is written;
//! * [`PgFeedAdmin`]: `spot-postgres`'s `adopt_position` and `open_generation`.
//!
//! The journal is untrusted input: `skip_verify` is not exposed here (the executor verifies
//! before applying, and the dry run says so).

#![forbid(unsafe_code)]

pub mod checkpoint;
mod error;
pub mod invariants;
pub mod projector;
mod request;
pub mod run;
pub mod sink;
pub mod writer;

pub use checkpoint::{Checkpoint, CheckpointStore, Fut, MemoryCheckpoints, PgCheckpoints, State};
pub use error::HostError;
pub use projector::{Projector, Projectors};
pub use request::{Mode, RebuildRequest, TargetSpec};
pub use run::{
    AdminAudit, Deps, FeedAdmin, InvariantResult, Invariants, JournalSide, PgFeedAdmin, RebuildFut,
    RebuildReport, Rebuilder, SiteGuard, plan_hash, rebuild_site,
};
pub use sink::{CheckpointingSink, SinkCounts};
pub use writer::{ImageCodec, NoSideEffects, ReplicaSink, WireCodec};
