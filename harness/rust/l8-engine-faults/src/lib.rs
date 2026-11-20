//! L8 fault injection at the document-engine level (WP-TST-08.T6).
//!
//! `spot-l8-faults` proved the storage promises survive a kill: acknowledged commits are durable,
//! an unacknowledged one is whole or absent. This crate attacks the promises one level up, the
//! ones the **document engine** makes with them: one lifecycle event per committed operation, a
//! re-sent command applied once (the command ledger of WP-DOC-04.T8), a posted document never
//! half posted, a cancelled-and-amended pair both or neither.
//!
//! # How a run goes
//!
//! 1. [`workload`] runs 8 workers over a `DocumentService`. **Every call carries a unique,
//!    deterministic command id** ([`log::command_id`]) and is recorded in the [`log::AttemptLog`].
//! 2. A fault ([`scenarios`]) kills PostgreSQL or cuts commit replies. A call whose commit state
//!    is unknown is **indeterminate** ([`classify`]).
//! 3. Every indeterminate call is **re-sent with the same command id and arguments** until the
//!    engine answers for good. It must answer the original result (a ledger hit) or run once now.
//! 4. [`oracle`] builds the L7 engine oracle from the resolved log, and the L7 engine invariants
//!    judge it; [`audit`] adds the engine durability rules a to f.
//!
//! # What is not covered
//!
//! Power loss and torn pages (WP-PRD-05); the job engine itself over PostgreSQL (WP-TST-08.T7).
//! A method call has no ledger row (the ledger covers writes, not methods), so an indeterminate
//! method call is settled by a read-back, and its worker is the only writer of its Task.

pub mod audit;
pub mod classify;
pub mod core;
pub mod decorate;
pub mod log;
pub mod oracle;
pub mod scenarios;
pub mod workload;

pub use audit::{
    CallOutcome, CallRecord, Finding, annotate, audit_documents, audit_events, audit_log,
    call_ledger,
};
pub use classify::{Class, classify};
pub use log::{Attempt, AttemptLog, command_id};
pub use scenarios::{Config, Kind, ScenarioOutcome};
