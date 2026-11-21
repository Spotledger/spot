//! `spot-ai-load` (WP-AI-05.T2): does a person feel twenty agents?
//!
//! WP-AI-05.T1 proved the admission control, the AIMD controller, fairness and cancellation
//! against a simulated database. This harness runs the same code against a real PostgreSQL
//! with two pools, one for the human users and one for the dedicated `ai` role, and measures
//! what the humans experience in three phases:
//!
//! 1. **baseline**: the human load alone (reads, filtered lists and one report-shaped
//!    aggregate);
//! 2. **with_agents**: the same human load plus agents looping `query` tool calls of mixed cost,
//!    including a deliberately expensive scan and a call that exceeds its deadline;
//! 3. **overload**: three times as many agents, a quarter of them runaways whose every call
//!    exceeds its deadline, so admission must queue, shed and trip breakers.
//!
//! It then probes how long admission takes to recover its configured level, polls
//! `pg_stat_activity` for statements still running after cancellation, and drops the role and
//! the schema (also when the run failed). [`budget::evaluate`], a pure function with its own
//! tests, judges the [`report::Report`] against `budgets.toml`.
//!
//! The full run is `#[ignore]`d and prints a JSON report; the short mode runs wherever
//! `SPOT_TEST_PG_URL` names a server. See the AI load-test notes.
//!
//! This is test code: no production crate depends on it. It measures with a monotonic clock
//! (`Instant`) because latency is what it measures, and reads no wall clock. The fake agent
//! provider is a deterministic call plan ([`calls::plan`]); no model is called.

#![forbid(unsafe_code)]

pub mod agent;
pub mod budget;
pub mod calls;
pub mod cancel;
pub mod clock;
pub mod config;
pub mod error;
pub mod fixture;
pub mod human;
pub mod load;
pub mod report;
pub mod run;
pub mod sampler;
pub mod stats;

pub use budget::{Budgets, evaluate};
pub use config::RunConfig;
pub use error::HarnessError;
pub use report::Report;
pub use run::run;
