//! The PostgreSQL [`JobStore`](spot_jobs::JobStore): durable jobs (WP-PLT-06.T2).
//!
//! [`PgJobStore`] keeps jobs, the idempotency index, the outbox event dedupe table and the
//! outbox intake position in PostgreSQL, so a node that dies mid-job loses nothing: its leases
//! lapse, [`reap_expired`](spot_jobs::JobStore::reap_expired) returns the work to the queue and
//! another node finishes it.
//!
//! This is not a document store: it owns its tables (`spot_job*`, created by
//! [`PgJobStore::prepare`]) and uses the pool it is given, so queue consumption can use
//! `FOR UPDATE SKIP LOCKED` directly (the storage contract's `Wait::SkipLocked`).
//!
//! # Rules the store keeps
//!
//! - **The database clock decides leases.** `claim`, `heartbeat` and the retry delay after a lost
//!   worker use `now()` of the server; the `now` argument of the trait is ignored there, so a
//!   node with a wrong clock can neither shorten nor stretch a lease. The one exception is
//!   [`reap_expired`](spot_jobs::JobStore::reap_expired), which reaps leases that lapsed at
//!   `max(database now, caller's now)`: the caller may only look *later* than the database,
//!   which the contract suite needs (it simulates elapsed time); production code calls
//!   [`PgJobStore::reap_lapsed`], which uses the database clock alone.
//! - **Claims are fenced.** Every claim takes a new `claim_token` from one sequence. `complete`,
//!   `heartbeat` and reaping match on it; a stale token changes nothing and fails with
//!   `job.claim_lost`.
//! - **Context writes are fenced too.** `merge_context` locks the job's row only while the
//!   claim token and worker still match, merges in Rust (shared limits: 64 KiB, 256 keys) and
//!   writes in the same transaction, so a stale claim writes nothing and two merges never lose
//!   a key. The context is job metadata, never a secret store.
//! - **`attempt` counts started attempts** (1-indexed, as `Job::attempt` documents): `claim`
//!   adds one. A lost worker therefore costs an attempt; a job whose attempts are all used is
//!   dead-lettered when its lease is reaped, so a job that crashes every worker cannot loop
//!   forever.
//! - **Every statement is bounded** (ADR-045): claims take at most `limit` rows, reaping and
//!   retention work in batches of [`BATCH`] rows, each in its own transaction.
//! - **Wake-up is polling.** The worker claims every 100 ms (`Worker::run`); the store never
//!   relies on `LISTEN` or session locks.
//!
//! # Per-site running cap
//!
//! [`PgJobStore::with_max_running_per_site`] limits how many jobs of one site run at once. The
//! claim statement counts the site's `running` rows and claims only up to the cap. Under READ
//! COMMITTED two claim statements racing could both count before either updates, so with a cap
//! set every claim call first takes a transaction-scoped advisory lock (`pg_advisory_xact_lock`,
//! released at commit, not a session lock) that serialises the claims of that store. The cap is
//! exact, never exceeded; the price is that claims queue behind each other while it is set. A
//! site at its cap can still crowd the candidate window of one claim call (the window is four
//! times the requested limit); jobs of other sites are then claimed by a later call.
//!
//! # Query plans
//!
//! The claim, idempotency, lease, retention and token lookups each have a partial index (see
//! [`schema`]); `tests/plans.rs` asserts from `EXPLAIN` that they are used.

#![warn(missing_docs)]

mod convert;
mod retention;
pub mod schema;
pub mod sql;
mod store;

pub use retention::{PurgeReport, Retention};
pub use store::PgJobStore;

/// Rows touched per statement by reaping and retention (ADR-045: bounded transactions).
pub const BATCH: u32 = 500;
