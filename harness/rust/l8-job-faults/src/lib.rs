//! L8 fault injection for the job pipeline (WP-TST-08.T7).
//!
//! The platform says a job enqueued by a document operation runs **if and only if that operation
//! committed**, and runs exactly once as to effect, even when the machine dies at the worst
//! moment. The claim crosses five components (the engine's transaction, the outbox, the job
//! intake, `spot-jobs-pg`, the handler), and was tested only in pieces. This crate runs the whole
//! path on a scratch cluster that it kills.
//!
//! - [`pipeline`]: the real parts on one scratch database: the document engine with a step that
//!   enqueues a job in the operation's own transaction, the intake (`spot_jobs::process_batch`),
//!   [`spot_jobs_pg::PgJobStore`], and a pool of 6 workers whose handler writes **one row in an
//!   `effects` table, in its own transaction, with `ON CONFLICT DO NOTHING`**.
//! - [`checks`]: the checks (a) to (e), pure functions of the truth and what the store holds, so
//!   they are tested without PostgreSQL.
//! - [`scenarios`]: the five kills.
//!
//! # Truth
//!
//! Every operation creates a Time Entry whose id the client chooses, so after a lost reply the
//! command ledger is not needed: the row's existence says whether the operation committed. The job
//! of that operation has the document id as its idempotency key.
//!
//! # Not covered
//!
//! SIGKILL is not power loss (WP-PRD-05). The committed-steps done markers (`JobStoreMarkers`,
//! check (f) of the brief) are not exercised: the handler here is the effect handler, not the
//! engine's `sys:committed_steps` runner; see the report.

pub mod checks;
pub mod pipeline;
pub mod scenarios;

pub use checks::{JobView, Truth, TruthOp, Violation, check_all, check_f};
pub use scenarios::{Config, Kind, Outcome};
