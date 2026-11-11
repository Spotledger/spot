//! Running the `*_committed` steps of a document write from the job engine (WP-DOC-04.T4;
//! ADR-51 phase 4, ADR-53).
//!
//! `spot_document::engine::DurableDelivery` writes one `sys:committed_steps` job per document
//! write in the operation's own transaction. [`CommittedStepsHandler`] is the job's handler: it
//! rebuilds the invocation the operation ran under (actor, site, command id and the operation's
//! original instant, never the wall clock), reads the document the job names, and runs each
//! listed point **in order** through the engine.
//!
//! # Per-step progress
//!
//! After each step the handler writes a done marker `{point}` through [`StepMarkers`]. A retry
//! after a crash (or a `Retry` answer) resumes at the first step not marked done and never
//! reruns a finished one.
//!
//! **At-least-once edge.** A crash between a step's side effect and its marker reruns that one
//! step. A step authored for the `*_committed` points therefore **must be idempotent on the
//! command id** (`ctx.invocation().command_id`): sending a mail, calling a connector or
//! notifying a room has to be recognisable as a repeat. This is the one edge where a step can
//! run twice; no other path reruns a step.
//!
//! # Failure policy
//!
//! A failed step is recorded in `Engine::committed_failures` as it always was. The job does not
//! fail for it:
//!
//! - a **retryable infrastructure error** (`AppError::retryable`, class `Unavailable`) ends this
//!   run with `HandlerResult::Retry` and no marker: the queue's backoff reruns the job, which
//!   resumes at that step;
//! - **any other error** (a business refusal, a bug) is logged by the engine, the step's marker
//!   is still written, and the next step runs. A step is never retried forever.
//!
//! # Ordering
//!
//! `spot-jobs` has no per-key serialisation, so ordering is enforced here: a job carries its
//! document's version, and the handler answers `Retry` while any job of the same site for the
//! same document with a lower position (version, deletion after the write it deletes) is still
//! pending, running or retrying. A dead or cancelled predecessor does not block. The check lists
//! the site's jobs of the job's queue; a store with an index on the arguments would make it
//! cheaper.
//!
//! # What the steps see
//!
//! The job carries no field value, so the document is read when the step runs and may be newer
//! than the write that caused the job (a later write has its own job, run after this one). A
//! step for a deletion, or for a document deleted since, sees the document's id and DocType
//! with no fields.
//!
//! # Isolation
//!
//! The handler resolves the runner from the job's own site ([`RunnerFor`]); a job whose
//! arguments name another site, or a site nobody serves, fails and runs nothing.

#![warn(missing_docs)]

mod handler;
mod markers;

pub use handler::{CommittedStepsHandler, RunnerFor, SingleSite, StepRunner};
pub use markers::{InMemoryMarkers, MarkerError, StepMarkers};
