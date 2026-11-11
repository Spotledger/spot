//! `spot-bpm-tasks-pg` — the PostgreSQL [`TaskStore`](spot_bpm_tasks::TaskStore) for the task
//! inbox (WP-BPM-05.T6, ADR-49).
//!
//! `spot-bpm-tasks` ships only the in-memory store, so approval tasks vanish on restart. This
//! crate keeps them: [`PgTaskStore`] implements `TaskStore` over `spot_bpm_task` and the
//! append-only `spot_bpm_task_event` (see [`schema`]), created by [`PgTaskStore::prepare`].
//!
//! # The sync trait and the bridge
//!
//! `TaskStore` is synchronous and PostgreSQL is not. The bridge is the shared, bounded
//! `spot-pg-bridge` (ADR-082 addendum 1): the caller's thread blocks on a channel while the
//! statement runs on the bridge's own runtime, with a deadline, a bound on work in flight, and
//! a statement cancel on timeout. This crate writes no `block_on` of its own.
//!
//! **Fail closed, never deadlock.** A call from a thread that belongs to any tokio runtime
//! (a current-thread runtime, where blocking would deadlock, but also a multi-thread worker or
//! its blocking pool) is refused with `bpm_tasks.store_failed` naming `pg_bridge.wrong_thread`;
//! nothing is run. A host drives the task service from a plain thread of its control or job
//! process. `tests/concurrency.rs` calls the store from many threads at once and from inside a
//! current-thread and a multi-thread runtime.
//!
//! # What the store keeps
//!
//! * One row per task, keyed `(tenant, task_id)`, unique on `(tenant, case_id, node)`: a
//!   second `create` for a Case and node returns the stored task and writes nothing.
//! * [`TaskStore::commit`](spot_bpm_tasks::TaskStore::commit) is **one statement**: the version
//!   check, the task update and the event insert. A stale version updates nothing and inserts
//!   nothing ([`TasksError::VersionConflict`](spot_bpm_tasks::TasksError)); two racing commits
//!   have exactly one winner and the loser leaves no event.
//! * The event table is append-only: a trigger refuses `UPDATE`, `DELETE` and `TRUNCATE`.
//!
//! # Security
//!
//! * **Tenant isolation.** The tenant is a constructor argument; every statement filters on it
//!   as parameter `$1`, and no method takes a tenant. Both tables carry it in their keys.
//! * **No injection.** All statements are fixed text in [`sql`] with bound parameters.
//! * **Fail closed.** A backend failure is `StoreFailed`, never an empty result; a row that
//!   disagrees with its own columns or cannot be decoded is an error, not skipped.
//! * **Quiet errors.** An error names the store and the SQLSTATE class only: no row content,
//!   comment, form value or key reaches a message or a log (this crate logs nothing).
//! * **No partial write.** A refused write (stale version, wrong identity, backend failure)
//!   leaves neither a changed task nor an event; the caller records the refusal.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, exactly as the other `*-pg` crates do.

#![forbid(unsafe_code)]

mod codec;
pub mod error;
pub mod schema;
pub mod sql;
mod store;

pub use error::{BAD_SETUP, MIGRATION_FAILED, SetupError};
pub use store::{PREPARE_DEADLINE, PgTaskStore, READ_DEADLINE, WRITE_DEADLINE};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-tasks-pg";
