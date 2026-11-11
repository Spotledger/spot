//! The scheduled journal audit, wired (WP-JRN-07.T5, ADR-061 s8).
//!
//! `spot-journal-audit-run` audits **one** tenant against a live side and a scratch side it is
//! handed. This crate calls it on a schedule for every tenant, turns the result into conditions
//! and offers the platform job that does so. It knows no database: the tenants, the audit's
//! inputs and the result rows come through the traits of [`providers`], so the PostgreSQL sides
//! (WP-JRN-07.T6) and the host plug in without a change here.
//!
//! * **Cadence**: [`Cadence::Weekly`] or [`Cadence::Daily`] (tenants on the `always` replay
//!   policy; the directory says which). [`due`] is pure.
//! * **Isolation**: [`run_due`] runs each due tenant on its own. An error, a refusal or a panic
//!   for one tenant is that tenant's [`TenantOutcome::Failed`] and never stops the others.
//! * **Conditions**: a clean run clears `JOURNAL_AUDIT_MISMATCH` and `JOURNAL_AUDIT_FAILED`; a
//!   mismatch raises `JOURNAL_AUDIT_MISMATCH` naming the first differing record (entity and id
//!   and the kind of difference, never content); a failed run raises `JOURNAL_AUDIT_FAILED`
//!   naming the stage and the failing crate's code. Both are red, subject = the tenant.
//! * **Idempotence**: a tenant whose result was recorded in the current period is not due.
//!   [`ResultSink::record`] must make the directory return [`TenantResult::started_millis`] as
//!   the tenant's `last_run_millis`.
//! * **The job**: [`handler`] runs a pass on the blocking pool and registers under [`JOB_NAME`].
//!
//! # Wiring order (the host)
//!
//! 1. Build the [`TenantDirectory`], the [`AuditInputs`] and the [`ResultSink`].
//! 2. Build the registry from `spot_sync_conditions::catalogue()` (it holds
//!    `JOURNAL_AUDIT_MISMATCH`), call [`register_conditions`] on it (it adds
//!    `JOURNAL_AUDIT_FAILED`), and build the `ConditionEngine` from it with the audit sink of
//!    the journal.
//! 3. `let job = Arc::new(AuditJob::new(directory, inputs, sink, clock, engine));`
//! 4. `registry.register(JOB_NAME, handler(job));` on the `JobHandlerRegistry`.
//!
//! What the host adds: the **schedule entry** that enqueues [`JOB_NAME`] (a platform-scoped job,
//! so not a manifest `[[schedules]]` entry, which is app-scoped: use `spot-scheduler`'s slot and
//! `spot_jobs::enqueue_scheduled`) and the real provider implementations. Run the entry at least
//! as often as the daily cadence; tenants that are not due are skipped.
#![forbid(unsafe_code)]

mod cadence;
mod conditions;
mod error;
mod job;
pub mod memory;
mod providers;
mod run;

#[cfg(test)]
mod tests;

pub use cadence::{Cadence, DAY_MILLIS, TenantAuditPlan, due};
pub use conditions::{
    JOURNAL_AUDIT_FAILED, JOURNAL_AUDIT_MISMATCH, SUBJECT_KIND, register_conditions,
};
pub use error::AuditJobError;
pub use job::{AuditJob, JOB_NAME, SyncClock, handler};
pub use providers::{AuditInputs, ResultSink, TenantDirectory, TenantInputs};
pub use run::{FailureInfo, RunSummary, TenantOutcome, TenantResult, run_due};
