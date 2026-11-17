//! `spot-sync-loops`: the periodic loops a hosted pair of nodes runs (WP-RTB-02.T6).
//!
//! [`Loops::tick`] runs every **due** [`Step`]; a step that errors or panics never stops the
//! others, each has its own interval, and each is bounded by `max_items` so one tick's cost is
//! bounded. Steps raise and clear conditions through a [`ConditionSink`] (the conditions service,
//! whose debounce and flap guard are not duplicated here). Time is an argument.
//!
//! Steps in this crate, every one a [`Step`] over a typed source port, with its own interval and
//! bound:
//!
//! | Step | Does | Responds on its own |
//! | --- | --- | --- |
//! | [`obligations`] | due soon, overdue, red; the ladder `notify_contact` before `suspend_node` | yes |
//! | [`liveness`] | `NODE_SILENT`, `LAG_HIGH` | no |
//! | [`freshness`] | `FRESHNESS_STALE`; the stale stamp | stamps |
//! | [`reconcile`] | digest per scope, `DIVERGENCE`; says "not available" without a provider | no |
//! | [`healing`] | scoped mends with an attempt budget, then `HEAL_BUDGET_EXHAUSTED` red | mends |
//! | [`closure`] | repair pulls for bundles on the repair list, `MISSING_DEP` | pulls |
//! | [`quarantine`] | `QUARANTINED` for open items (retries nothing) | no |
//! | [`certificate`] | `CERT_EXPIRING` (30 days amber, 3 red) | no |
//! | [`series`] | samples numbers into the [`SyncHealthView`] | no |
//!
//! Every automatic response writes one [`Decision`] (`sys_sync_decision`) through a
//! [`SharedDecisions`] sink; a step that only raises a condition writes none.
//!
//! No automatic response is invented: where the ladder names an automatic response it is a typed
//! [`Responder`] call that records a decision; where it says "person" nothing is done but keeping
//! the condition open.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audit;
pub mod certificate;
pub mod closure;
pub mod freshness;
pub mod healing;
pub mod health;
pub mod liveness;
pub mod obligations;
pub mod quarantine;
pub mod reconcile;
mod runner;
pub mod series;

pub use audit::{Decision, DecisionSink, MemoryDecisions, SYS_SYNC_DECISION, SharedDecisions};
pub use health::SyncHealthView;
pub use runner::{
    ConditionSink, Loops, Raised, Severity, Step, StepError, StepOutcome, Summary, TickReport,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-loops";
