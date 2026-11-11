//! The PostgreSQL [`FixJournal`](spot_fix_pipeline::FixJournal) (WP-AI-08.T2, ADR-082 class C).
//!
//! `spot-fix-pipeline` writes one `JournalRecord` **before** it performs each transition or step
//! (a write-ahead log of agent actions) and ships an in-memory journal. [`PgFixJournal`] makes
//! the log durable, so a restart after a crash can fold it with `recover` and either finish the
//! fix or roll it back. The audit property matters more than speed: the log can only grow.
//!
//! # Where it may be used
//!
//! The trait is synchronous with no async alternative, so the journal reaches the database
//! through the shared [`spot_pg_bridge::Bridge`] (there is no `block_on` here). That is
//! legitimate only off the request path: **use it from a control-plane process, a CLI command or
//! a job worker, never from a request handler**, and never inside a transaction an async store
//! holds (it takes its own pooled connection; holding one while waiting for another can exhaust
//! the pool). Called from a thread inside a tokio runtime the bridge refuses with
//! `pg_bridge.wrong_thread` and the journal returns `fix_pipeline.journal_failed` naming that
//! code. [`PgFixJournal::new`] refuses to be built on a current-thread runtime.
//!
//! # Semantics (the same as `InMemoryJournal`)
//!
//! * `append`: one transaction bumps the single head row (`spot_fix_journal_head`, which locks
//!   it) and inserts the entry with the new number. Numbers start at 1, are gapless and never
//!   repeat; a rolled-back append consumes none. The instant `at` is the caller's argument.
//! * `entries(fix)`: one fix's records in sequence order, read in pages in one snapshot.
//! * `all`: every record in sequence order, at most [`ALL_CEILING`]; a longer journal fails with
//!   `fix_pipeline.journal_failed` instead of truncating. [`PgFixJournal::page_after`] streams a
//!   long journal in pages of at most [`PAGE`].
//! * A row that no longer deserialises is `fix_pipeline.journal_failed`, never skipped.
//! * Every database error or bridge refusal is `fix_pipeline.journal_failed`, with only the
//!   bridge code or the SQLSTATE in the text: never SQL, a connection string or a record. A
//!   timed-out append may or may not have been applied; read the fix's entries again before
//!   retrying (the engine's `recover` does).
//!
//! # Append-only
//!
//! See [`schema`]: the application role `spot_fix_journal_app` has `SELECT, INSERT` only, a
//! trigger raises on `UPDATE`, `DELETE` and `TRUNCATE` for every role including the owner.
//!
//! # Bridge sizing
//!
//! See the README: appends serialise on one row, so more than a couple of callers in flight buy
//! nothing; `max_in_flight` of 2 on a pool of 4 is a sound default.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod schema;
mod store;

pub use error::{SetupError, codes};
pub use store::{
    ALL_CEILING, APPEND_DEADLINE, ENTRIES_CEILING, PAGE, PAGE_DEADLINE, PgFixJournal, READ_DEADLINE,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-fix-pipeline-pg";
