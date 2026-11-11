//! The journal commit driver (WP-JRN-04.T2, ADR-061 s4 and s10).
//!
//! `spot-journal-durability` decides when an acknowledgement may be released (a pure state
//! machine) and `spot-journal-relay` makes batches durable. This crate is the small driver that
//! feeds the first with the results of the second and gives the commit path one future per
//! commit.
//!
//! # The flow of one commit
//!
//! 1. [`CommitDriver::submit`] queues the request and returns a [`CommitTicket`]. A request
//!    whose `command_id` the driver or the gate has seen is never queued again: it is answered
//!    from the gate's outcome (or attached to the commit still in flight).
//! 2. The caller commits to the database and calls [`CommitDriver::local_commit_durable`]. No
//!    ticket is resolved with `Acked` before this call; the gate learns of a commit only then.
//!    Under `replica-sync` the caller also reports [`CommitDriver::standby_confirmed`].
//! 3. [`CommitDriver::step`] (or [`CommitDriver::run`], which calls it when needed) ships the
//!    queue as group-commit batches: requests that arrived within `group_window_millis` of the
//!    oldest, up to `max_batch`, form one batch and one object-store round trip. A durable chunk
//!    is fed to the gate, which releases every ticket it covers in sequence order.
//!
//! # When the store is down
//!
//! A failed shipment becomes `ChunkFailed` for the gate and the batch stays queued. With
//! `degrade` the gate acknowledges the commit on its local durability and [`CommitDriver::status`]
//! exposes the RPO state; with `halt` new submits are answered `Refused`. The driver probes the
//! store every `retry_millis` by resuming the relay; a successful probe is `StoreReachable(true)`
//! and shipping continues. A failure a retry cannot cure (another writer, a broken chain) stops
//! shipping and refuses new commits with `journal_commit.relay_stopped`.
//!
//! # Time, tenancy, secrets
//!
//! One driver per tenant. Time comes only from the [`Runtime`] argument, so tests use a virtual
//! clock. A `command_id` is only compared, never logged or put in an error; the crate logs
//! nothing and entry lines never appear in an error.

#![forbid(unsafe_code)]

mod driver;
mod error;
mod runtime;
mod seam;
mod settings;
mod ticket;

#[cfg(test)]
mod tests;

pub use driver::{CommitDriver, DriverStatus};
pub use error::CommitError;
pub use runtime::Runtime;
pub use seam::{ChunkRelay, ShipEntry, ShipError, Shipped};
pub use settings::CommitSettings;
pub use ticket::{CommitAnswer, CommitRequest, CommitTicket, Observer};
