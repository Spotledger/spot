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
//! # A failed local commit
//!
//! The caller reports a database commit that failed or rolled back with
//! [`CommitDriver::local_commit_failed`] (instead of `local_commit_durable`). The ticket resolves
//! [`CommitAnswer::Withdrawn`] and the command may be retried; the gate never acknowledges it.
//! What becomes of the journal entry depends on where it is:
//!
//! * **Queued, and the newest submitted request:** removed from the queue; its sequence number is
//!   released, so the retry takes it again.
//! * **Queued with a later request behind it:** the relay accepts only strictly consecutive
//!   positions, so dropping it would leave a gap the later entries cannot be shipped across. It
//!   stays and ships; it is counted like a shipped entry.
//! * **Already shipped (or in flight):** an entry cannot be unshipped. The events contract has no
//!   void or compensating entry, so none is invented: nothing is shipped, the entry is counted in
//!   [`DriverStatus::withdrawn_shipped`] (with the lowest sequence number) and the driver goes on
//!   with later sequence numbers in order. Consumers of the journal can see an entry of a commit
//!   that did not happen until a `journal_void` entry exists (an open decision).
//! * **Confirmed locally and waiting for its acknowledgement:** the gate withdraws it
//!   (`Event::LocalCommitFailed`); the sequence number stays consumed and the entry is counted as
//!   above. An acknowledgement is never taken back: the call fails with `unknown_seq`.
//!
//! A request whose `local_commit_durable` or `local_commit_failed` does not arrive within
//! `CommitSettings::local_commit_deadline_millis` is withdrawn exactly like a failed one and
//! counted in [`DriverStatus::expired_local_commits`]; the driver wakes for it. A request is
//! expired once the clock is past `submitted + deadline`, so a confirmation at exactly the
//! deadline is still taken (until the next `step` has expired it).
//!
//! # Generations
//!
//! The acknowledgement gate keys every commit on `(generation, seq)` ([`spot_journal_durability::Position`]),
//! because `seq` restarts at 1 in the generation a fork or restore opens. One driver writes one
//! generation, the relay's ([`CommitDriver::generation`]), so this crate's own API (requests,
//! tickets, `local_commit_*`, `standby_confirmed`) stays keyed by `seq` and the driver adds that
//! generation when it talks to the gate. [`DriverStatus::rpo_breach`] carries the gate's
//! [`spot_journal_durability::RpoBreach`], whose `unshipped_from` is now a `Position`. A
//! confirmation the gate refuses (stale generation, out of order, reused command id) is never
//! dropped silently: its ticket resolves [`CommitAnswer::Withdrawn`] and
//! [`DriverStatus::rejected_commits`] counts it.
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
