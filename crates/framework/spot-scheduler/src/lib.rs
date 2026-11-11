//! Scheduling for manifest schedules (WP-PLT-07).
//!
//! What a manifest's `[[schedules]]` entry fires and who may run it:
//!
//! - [`Schedule`]: a [`ScheduleSub`](spot_manifest::ScheduleSub) plus an IANA zone, with
//!   [`Schedule::next_after`] and [`Schedule::due_between`], DST-correct: a job scheduled in a
//!   spring-forward gap fires once, at the first valid instant after the gap; on the fall-back
//!   day it fires once, at the first occurrence; cron's day-of-month/day-of-week union rule
//!   holds as cron 0.12 defines it, see [`Schedule`] and [`crate::cron_union`].
//! - [`MissedRunPolicy`]: what happens to runs missed while nobody was ticking — `skip`,
//!   `run_once`, `catch_up(max)` — producing [`RunSlot`]s whose
//!   [`RunSlot::deduplication_key`] makes a double enqueue harmless.
//! - [`lease`]: `LeaseStore` with strictly-increasing fencing tokens per name;
//!   [`FencedWrite`] rejects a write from a stale token; [`InMemoryLeaseStore`] for tests and
//!   single-node hosts.
//! - [`Scheduler`]: one node's `tick` — acquire the schedule-leader lease, compute due slots
//!   since the high-water mark, hand them to a callback, and advance the mark only after the
//!   handover succeeded; losing the lease abandons the tick. Failover produces no duplicate
//!   and no missing slot.
//!
//! Everything is synchronous and takes times from a [`Clock`](spot_core::Clock); no real
//! sleeping, no system-clock reads, no floats.

#![forbid(unsafe_code)]

pub mod cron_union;
pub mod error;
pub mod fence;
pub mod lease;
pub mod policy;
pub mod schedule;
pub mod scheduler;
pub mod slot;
mod wall;

pub use cron_union::{DAY_OF_WEEK_SUNDAY, day_matches};
pub use error::{Error, Result, SCHEDULER_ERROR_PREFIX};
pub use fence::{FenceGuard, FencedWrite};
pub use lease::{FencingToken, InMemoryLeaseStore, Lease, LeaseId, LeaseStore, still_holds};
pub use policy::{MissedRunPolicy, PLAN_DUP_LIMIT, PLAN_FROM};
pub use schedule::{MAX_CRON_LOOKAHEAD_DAYS, Schedule, ScheduleBuilder, TimeZone};
pub use scheduler::{
    HighWater, HighWaterStore, InMemoryHighWater, Scheduler, SchedulerConfig, SlotSink,
    leader_lease_name,
};
pub use slot::{RunSlot, SlotOrigin, SlotRef};

/// The crate's own name, for error messages and test output.
pub const CRATE_NAME: &str = "spot-scheduler";
