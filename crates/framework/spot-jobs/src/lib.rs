//! The job engine: persistent jobs with queue policies, retries, idempotency keys,
//! enqueue-after-commit, fenced worker loops and outbox intake.
//!
//! A job is work that must run later, exactly as decided in a committed transaction, survive a
//! crash, and never be lost or silently doubled. This crate delivers the engine's logic over a
//! [`JobStore`] trait with an in-memory store. See [`model`] for the job shape and [`worker`] for
//! the job handler interface.
//!
//! # Claim gate
//!
//! A worker claims on a 100 ms tick and relies on the store's claim fencing plus its renew/stop
//! loop. That fencing is the last line of defence: a lease lost without the loop noticing (a long
//! pause, a network partition) lets two hosts act on one job for up to a third of the lease. A
//! host that also owns a coordinator lease therefore attaches an optional [`ClaimGate`] with
//! [`Worker::with_claim_gate`]; the worker asks it before every claim, skips the tick when it
//! answers `false` and keeps running (the host may regain the lease, and the host decides when to
//! stop the worker). Without a gate the worker behaves exactly as before.
//!
//! The crate only asks. Deciding `true` or `false` from the coordinator lease -- the other half of
//! the contract -- belongs to the host that wires the worker (WP-SEC-12.T5).

#![warn(missing_docs)]

//! Feature flags:
//! - `testing`: Contract tests and in-memory store.

// Lets `contract.rs`, which the PostgreSQL backend also compiles into its own test through
// `#[path]`, name this crate's modules the same way from both places.
#[cfg(feature = "testing")]
extern crate self as spot_jobs;

pub mod backoff;
pub mod error;
pub mod intake;
pub mod model;
pub mod policy;
pub mod store;
pub mod worker;

#[cfg(feature = "testing")]
#[allow(clippy::unwrap_used)]
pub mod contract;

pub use backoff::Backoff;
pub use error::{Error, Result};
pub use intake::{enqueue_scheduled, process_batch, IntakeReport, JobEnqueued, RejectedEvent};
pub use model::{
    merge_context_value, Claim, Job, JobId, JobState, NewJob, MAX_CONTEXT_BYTES, MAX_CONTEXT_KEYS,
};
pub use policy::{QueueName, QueuePolicy};
pub use store::{InMemoryJobStore, JobStore};
pub use worker::{BoxFuture, ClaimGate, FinalAttemptHook, JobContext, Worker};

/// The topic name for job enqueued events in the outbox.
pub const SYS_JOB_ENQUEUED_TOPIC: &str = "sys_job_enqueued";
