//! The PostgreSQL audit store (WP-AI-11.T2; ADR-061, ADR-070).
//!
//! [`spot-audit-chain`](spot_audit_chain) proves a chain of audit records was not altered; this
//! crate is the durable, verifiable place it lives, plus the first producer and consumers of the
//! chain:
//!
//! - [`PgAuditStore`] implements [`AuditStore`](spot_audit_chain::AuditStore) over a
//!   caller-supplied `deadpool_postgres::Pool`. Append is a compare-and-set on the head, so two
//!   writers can never fork the chain; [`PgAuditStore::prepare`] creates the tables, the
//!   `spot_audit_app` role and the guard trigger.
//! - [`AgentDecisionAudit`] turns an agent [`DecisionRecord`](spot_agent_tiers::DecisionRecord)
//!   into an audit record (`kind = "agent.decision"`, the action name and decision code only)
//!   and pushes it to a bounded channel a [`AgentDecisionAudit::drain`] future appends from.
//!   [`AuditedAuthoriser`] refuses an action whose audit cannot be recorded, under the default
//!   fail-closed policy.
//! - [`VerifyJob`] is the body of a scheduled job: it verifies the chain incrementally from the
//!   last good checkpoint, signs a new one, and reports a [`ChainBroken`] on failure.
//! - [`digest`] builds a human review digest of a range: counts by kind and actor class, every
//!   refusal, every unmasked entry and the verification status.
//!
//! # The database cannot rewrite history
//!
//! `spot_audit_chain` is append-only in three layers. [`prepare`] `REVOKE`s `UPDATE`, `DELETE` and
//! `TRUNCATE` from the application role `spot_audit_app` — which is **never** the pool's login
//! role — and grants it only `SELECT` and `INSERT`. A guard trigger then refuses `UPDATE` and
//! `DELETE` even for the table's owner, unless the maintenance setting `spot.audit_maintenance`
//! is `on` (the documented restore path, and the only way a superuser may edit the table). The
//! chain itself makes any edit that slips through detectable ([`VerifyJob`]).
//!
//! The four tables of the lane appender's state follow the same least-privilege rule: the role may
//! append to the event and document indexes but never update or delete them, and may move the
//! bookmark and the site checkpoint forward; the grant table is in the README and in `sql.rs`.
//! [`verify`] reports it as `appender_tables` and `appender_grants`.
//!
//! # Creating the tables through the migrator
//!
//! [`TABLE_DDL`] holds the two `CREATE TABLE IF NOT EXISTS` statements, one per table, in the
//! shape `spot_pg_migrate::ExtraTable` accepts (the same statements [`prepare`] runs). The role,
//! the grants and the triggers cannot be an `ExtraTable` (the migrator only creates objects), so
//! [`install_guard`] installs them after the plan and [`verify`] reports, read-only, whether the
//! chain is fully protected. A host calls `verify` at start-up and refuses to serve on an
//! incomplete [`Protection`] ([`Protection::require_complete`]).
//!
//! # The durable appender state
//!
//! [`PgAppenderStore`] implements `spot_doc_audit::AppenderStore` (the lane appender's bookmarks,
//! event-id index, document index and site checkpoint) over a `deadpool_postgres::Pool`, scoped by
//! site. [`APPENDER_TABLE_DDL`] holds its four tables in the migrator's shape and
//! [`verify_appender`] reports, read-only, whether they are present. The port is async and
//! fallible: a failed call is returned as `docaudit.appender_state`, the appender ends the drain
//! and the job retries; see the [`PgAppenderStore`] documentation. The production store must commit
//! the chain entry and the event index together; WP-SEC-06.T7 closes that window by recovery.
//!
//! # Refused operations
//!
//! [`PgDenialWriter`] implements `spot_doc_audit::DenialWriter`: it writes one `sys_doc_audit`
//! outbox event per refused operation in a transaction of its own, for one site, with no value and
//! no message in the event. The audit denial sink drives it after the operation has rolled back.
//!
//! # No payload is logged
//!
//! A record never carries an action's arguments, and a verification failure never carries a
//! payload value: only a kind and a `seq`.

mod appender;
mod appender_sql;
mod appender_verify;
mod denial;
mod digest;
mod error;
mod guard;
mod job;
mod schema;
mod sink;
mod sql;
mod store;

pub use appender::{DEFAULT_DEADLINE, PgAppenderStore};
pub use appender_sql::{
    APPENDER_TABLE_DDL, BOOKMARK_TABLE, BOOKMARK_TABLE_DDL, DOC_INDEX_TABLE, DOC_INDEX_TABLE_DDL,
    EVENT_INDEX_TABLE, EVENT_INDEX_TABLE_DDL, SITE_CHECKPOINT_TABLE, SITE_CHECKPOINT_TABLE_DDL,
};
pub use appender_verify::{AppenderTables, verify_appender};
pub use denial::PgDenialWriter;
pub use digest::{Digest, DigestEntry, VerifyStatus, digest};
pub use error::{
    APPENDER_TABLES_MISSING, EVENT_CONFLICT, Error, PROTECTION_INCOMPLETE, Result, SITE_MISMATCH,
    TABLES_MISSING, UNAVAILABLE,
};
pub use guard::{Protection, install_guard, verify};
pub use job::{ChainBroken, VerifyJob, VerifyOutcome};
pub use sink::{
    AgentDecisionAudit, AuditUnavailable, AuditedAuthoriser, DECISION_KIND, DROPPED_COUNTER,
    DecisionReceiver, NoTelemetry, OverflowPolicy, Telemetry, audit_record_of, drain,
};
pub use sql::{
    APPLICATION_ROLE, CHAIN_TABLE, CHAIN_TABLE_DDL, CHECKPOINT_TABLE, CHECKPOINT_TABLE_DDL,
    MAINTENANCE_SETTING, TABLE_DDL,
};
pub use store::{PgAuditStore, prepare};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-audit-store";
