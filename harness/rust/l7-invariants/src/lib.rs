//! L7 invariant suite and parallel-load driver, storage level (WP-TST-07.T1).
//!
//! A suite that can pass on a broken system proves nothing, so this crate has two halves of equal
//! weight: the machinery (a registry of [`Invariant`]s, a seeded multi-worker [`LoadDriver`], a
//! client-side [`History`] and an exact [`Oracle`]) and the **seeded-corruption proofs** that show
//! each invariant fails when, and only when, its promise is broken.
//!
//! # How a run goes
//!
//! 1. [`LoadDriver::run`] seeds the store and runs a weighted mix of transactions on W workers,
//!    each at a declared isolation level, retrying serialization failures and lock timeouts within
//!    a [`driver::RetryPolicy`]. It records every operation and folds what committed into the
//!    oracle. Commits whose outcome is unknown are resolved by reading back a marker event.
//! 2. [`InvariantRegistry::check_all`] runs every registered invariant against a read-only
//!    snapshot of the store, the history and the oracle, never stopping at the first violation,
//!    and returns a [`Report`] (JSON and text).
//! 3. Apps add their own invariants with [`InvariantRegistry::register`].
//!
//! # Honest limits
//!
//! - The schedule of a run is a pure function of its seed ([`Schedule::generate`]). The
//!   interleaving of workers on real threads is not, so a replay repeats the plan, not every
//!   conflict.
//! - The feed invariants read the feeds after the workload stopped; run checks on a quiescent
//!   store.
//! - Needs a PostgreSQL server for the `pg` backend (`SPOT_TEST_PG_URL`); without one those
//!   tests print `SKIPPED` and, with `SPOT_REQUIRE_PG=1`, fail.
//!
//! # Profiles
//!
//! `SPOT_L7_PROFILE=smoke|standard|nightly` ([`Profile::from_env`]) selects 8 workers and about
//! 2,000 operations (the default, run by `cargo test`), 32 workers and 50,000, or 200 workers and
//! 1,000,000. The `spot-l7` binary runs any profile:
//! `spot-l7 run --backend fake|pg --profile P --seed N --report out.json [--history out.jsonl]`.

pub mod builtin;
pub mod corrupt;
pub mod driver;
pub mod faulty;
pub mod history;
pub mod invariant;
pub mod model;
pub mod oracle;
pub mod pg;
pub mod runner;
pub mod schedule;

pub use builtin::framework_registry;
pub use driver::{DriverConfig, LoadDriver, RetryPolicy, RunResult};
pub use history::{History, HistoryEntry, OpKind, Outcome};
pub use invariant::{CheckContext, Invariant, InvariantRegistry, Report, RunMeta, Violation};
pub use model::Model;
pub use oracle::Oracle;
pub use schedule::{Profile, Schedule};
