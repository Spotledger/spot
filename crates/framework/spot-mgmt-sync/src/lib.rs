//! spot-mgmt-sync: the sync operations as typed management operations
//! (WP-RTB-04.T2, ADR-067, ADR-062, ADR-44).
//!
//! "Every GUI action can show its equivalent command" holds only if an operation is data. The
//! sync surface had operators' verbs (enrol a node, suspend it, resend, retry or discard a
//! quarantined item, change a role) and no operations; this crate defines them, in a pure crate
//! like the Linux, database and journal modules (`spot-agent-linux`, `spot-agent-runtime`,
//! `spot-mgmt-journal`).
//!
//! # Shape
//!
//! - [`ids`] and [`catalogue`]: [`SyncModule`] registers eighteen `sync.*` operations with
//!   risk levels, closed enums, bounded integers and whole-value patterns. The three that
//!   destroy something (`sync.node.revoke`, `sync.quarantine.discard`, `sync.role.change`)
//!   need a `confirm` equal to their target.
//! - [`args`] and [`results`]: typed arguments and typed results. Results carry counts,
//!   positions, ids and reason codes, never record content. `sync.health` returns the
//!   `HealthView` of `spot-sync-health` unchanged. `sync.node.enrol` returns the one-time code
//!   once, in a wrapper whose `Debug` prints `<redacted>`; the invocation carries no secret.
//! - [`ops`]: [`SyncOps`], the executor trait the site host implements (WP-RTB-04.T3). An
//!   implementation is bound to one tenant and one caller, checks permission itself and fails
//!   closed, and audits refusals and failures as well as successes.
//! - [`dispatch`](mod@dispatch): maps a validated invocation to the method it names.
//! - [`actions`]: which operator actions of the conditions catalogue are operations here, and
//!   which are not (and why); a test keeps the two from drifting.
//!
//! # Purity
//!
//! No storage, clock, randomness, network, floats or `unsafe`. Signing and transport belong to
//! the management API server; permission checks to the executor.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod actions;
pub mod args;
pub mod catalogue;
pub mod dispatch;
pub mod error;
pub mod ids;
pub mod ops;
pub mod results;

pub use actions::{NOT_AN_OPERATION, OPERATOR_ACTIONS};
pub use args::{
    EnrolArgs, NodeArgs, NodeRole, PolicyShowArgs, Position, QuarantineDiscardArgs,
    QuarantineRetryArgs, ResendArgs, RoleChangeArgs,
};
pub use catalogue::{MODULE_NAME, SyncModule, build_catalogue, specs};
pub use dispatch::dispatch;
pub use error::{ExecError, ExecKind, OpError};
pub use ops::SyncOps;
pub use results::OneTimeCode;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mgmt-sync";
