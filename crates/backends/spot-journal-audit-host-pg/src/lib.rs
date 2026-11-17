//! The PostgreSQL side of the hosted journal audit (WP-RTB-01.T7, ADR-061 sections 8 and 9).
//!
//! `spot-journal-audit-job` documents its wiring order and says the PostgreSQL sides and the host
//! plug in; this crate is those sides:
//!
//! * [`PgTenantDirectory`], [`PgResultSink`] and [`PgAuditHistory`] over the result table
//!   `spot_journal_audit_result` ([`schema::DDL`]): counts, positions, outcome, and for a mismatch
//!   the identity of the first differing record (entity and id) and the kind of difference, never
//!   a field value. Fifty-two weeks are kept.
//! * [`SiteAuditInputs`]: the live side over the site's pool (one held read-only snapshot, the
//!   audit never writes the live database) and a scratch side in a **database of its own**
//!   (`spot_audit_<tenant>_<runid>`) on the same server, owned by a dedicated role that holds
//!   `CREATEDB` and nothing else ([`ScratchFactory::check_role`]; a superuser is refused with
//!   `journal_audit_host.role_too_powerful`). The database is dropped on every path
//!   ([`ScratchDatabase`]'s guard) and a crashed run's leak is swept ([`ScratchFactory::sweep`]).
//!   One tenant per run: the inputs refuse another tenant's name.
//! * [`RecoveryReportService`]: the per-tenant recovery report from real facts.
//! * [`SyncPg`]: the thread that lets the synchronous providers reach the database from the
//!   blocking pool (where `spot-pg-bridge` refuses).
//!
//! What the host supplies ([`JournalSource`], the fact traits, the replay policies) is in
//! `crates/framework/spot-cli/src/services/journal_audit.rs`.

#![forbid(unsafe_code)]

mod error;
mod inputs;
mod report;
mod results;
pub mod schema;
mod scratch;
mod worker;

pub use error::HostError;
pub use inputs::{JournalData, JournalSource, RunClock, SiteAuditInputs};
pub use report::{
    AuditHistorySync, BlobFacts, ExclusionRegistrySync, JournalFactsSync, KeyFactsSync,
    NoBlobCheck, RecoveryReportService, ReportSources,
};
pub use results::{AllWeekly, PgAuditHistory, PgResultSink, PgTenantDirectory, ReplayPolicies};
pub use scratch::{
    DATABASE_PREFIX, SWEEP_AFTER_MILLIS, ScratchDatabase, ScratchFactory, ScratchWithDatabase,
    database_name, judge_role,
};
pub use worker::SyncPg;
