//! `spot-bpm-pg` — the PostgreSQL [`CaseStore`] for the durable Business Process engine
//! (WP-BPM-02.T2, ADR-49).
//!
//! `spot-bpm-engine` (WP-BPM-02.T1) is the engine over traits: a Case *is* a `spot-dag-exec` run
//! plus the engine's bookkeeping (attempt outcomes, waits, child links, variables), and a
//! [`CaseStore`] keeps the two together. This crate makes that store durable:
//!
//! * [`PgCaseStore`] implements [`CaseStore`] and [`CaseStoreFactory`] over `spot_bpm_case`,
//!   `spot_bpm_case_var` and `spot_bpm_node_outcome` (see [`schema`]), created by [`PgCaseStore::prepare`].
//! * The `spot_bpm_node_outcome` row is keyed by the **idempotency key** `(case, node, attempt)`, so a
//!   replayed attempt whose outcome is already stored returns the stored outcome instead of
//!   storing a second one. That is the engine's exactly-once guard, made durable.
//! * [`PgCaseJobs`] wires the engine's next step to the real job engine (`spot-jobs-pg`), and
//!   [`FencedTimerPort`] wires a Case's waits to the scheduler's lease and fencing model
//!   (`spot-scheduler`).
//!
//! # What the store keeps
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_bpm_case` | the run header, the whole node map and the child links, plus the two flags `list_runnable` needs |
//! | `spot_bpm_case_var` | the Case's variables |
//! | `spot_bpm_node_outcome` | one row per node attempt: the engine attempt and the node's wait |
//!
//! Every table is **tenant-scoped**: a row carries its tenant and every query filters by it. A
//! store is built for one tenant ([`PgCaseStore::with_tenant`], default [`DEFAULT_TENANT`]).
//!
//! # The transaction it owns
//!
//! [`CaseStore::commit`] applies a whole [`CaseUpdate`](spot_bpm_engine::CaseUpdate) in **one**
//! database transaction: the DAG transition, the attempt outcome, the wait and the variables
//! commit or roll back together — the blueprint's "state and document commit or roll back
//! together", one layer up. The Case row is taken `FOR UPDATE`, so two commits of one Case
//! serialise and the optimistic version check cannot race.
//!
//! The node handler **never** runs inside that transaction: the engine stores a `Started`
//! attempt, calls the handler, and only then stores the outcome, so a handler that talks to the
//! outside world holds no store transaction open. `tests/transactions.rs` asserts it from
//! `pg_stat_activity`.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, exactly as the other `*-pg` crates do. In CI the Postgres service
//! sets it and the suite runs: a crash/restart chaos run over 500 Cases, a 10,000-Case scale run,
//! the exactly-once replay, the plan assertions and the two adapters.

#![forbid(unsafe_code)]

mod convert;
pub mod error;
mod jobs;
pub mod schema;
pub mod sql;
mod state;
mod store;
mod timer;

pub use error::MIGRATION_FAILED;
pub use jobs::PgCaseJobs;
pub use store::{DEFAULT_TENANT, PgCaseStore, PgCaseStoreFactory};
pub use timer::FencedTimerPort;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-pg";
