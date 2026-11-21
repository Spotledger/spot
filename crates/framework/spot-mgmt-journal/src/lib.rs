//! spot-mgmt-journal: the journal's operations as typed management operations
//! (WP-RTB-04.T1, ADR-061 sections 4, 6, 7 and 8, ADR-44).
//!
//! "Every GUI action can show its equivalent command" holds only if an operation is data. The
//! journal had operators' actions (take a snapshot, run the audit, rebuild, grant an auditor,
//! erase a subject) and no operations; this crate defines them, in a pure crate like the Linux
//! and database modules (`spot-agent-linux`, `spot-agent-runtime`).
//!
//! # Shape
//!
//! - [`ids`] and [`catalogue`]: [`JournalModule`] registers sixteen `journal.*` operations with
//!   risk levels, closed enums, bounded integers and whole-value patterns. The two dangerous
//!   ones are structurally hard to fire by accident: `journal.rebuild.run` needs the hash of a
//!   plan a person has seen, and `journal.subject.erase` needs the literal `--confirm erase`.
//! - [`args`] and [`results`]: typed arguments and typed results. Results carry counts,
//!   positions, ids and reason codes, never record content.
//! - [`ops`]: [`JournalOps`], the executor trait the site host implements (WP-RTB-04.T3). An
//!   implementation is bound to one tenant and one caller, checks permission itself and fails
//!   closed, and audits refusals and failures as well as successes.
//! - [`dispatch`](mod@dispatch): maps a validated invocation to the method it names.
//!
//! # Purity
//!
//! No storage, clock, randomness, network, floats or `unsafe`. Signing and transport belong to
//! the management API server; permission checks to the executor.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod args;
pub mod catalogue;
pub mod dispatch;
pub mod error;
pub mod ids;
pub mod ops;
pub mod results;

pub use args::{
    OnUnreachable, PolicySetting, Position, ReaderGrantArgs, ReaderRevokeArgs, RebuildMode,
    RebuildPlanArgs, RebuildRunArgs, RebuildTarget, SnapshotListArgs, SubjectEraseArgs,
};
pub use catalogue::{ERASE_CONFIRM, JournalModule, MODULE_NAME, build_catalogue, specs};
pub use dispatch::dispatch;
pub use error::{ExecError, ExecKind, OpError};
pub use ops::JournalOps;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mgmt-journal";
