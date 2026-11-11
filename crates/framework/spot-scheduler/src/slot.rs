//! [`RunSlot`]: one planned run of one schedule, with the key that makes a double enqueue
//! harmless.

use jiff::Timestamp;
use serde::Serialize;
use spot_manifest::{Queue, ScheduleSub};
use std::time::Duration;

use crate::schedule::Schedule;

/// Where a planned run came from, for logs and the simulation's invariant checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotOrigin {
    /// The slot's scheduled time was reached normally (the tick at or after it).
    Due,
    /// The slot is one collapsed representative of missed runs
    /// ([`MissedRunPolicy::RunOnce`](spot_manifest)) or the first of a caught-up burst.
    Missed,
    /// A catch-up run within the policy's limit.
    CatchUp,
}

/// One run the scheduler decided must happen: when it was scheduled for and the idempotency key
/// the job engine enqueues it under.
///
/// The key is `<app>:<schedule>:<scheduled_for RFC 3339>` (the brief fixes the shape). Because
/// the same missed run produces the same key on every node and on every re-plan, a second
/// enqueue is dropped by the job engine's idempotency check and a failover double-computes the
/// slot harmlessly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct RunSlot {
    /// The schedule's name (already qualified `<app>:<schedule>` by the manifest's own
    /// uniqueness rule; the app prefix comes from [`crate::slot::RunSlot::app_prefix`]).
    pub schedule: String,
    /// The scheduled wall-clock time, in UTC, as the job engine's `not_before`.
    pub scheduled_for: Timestamp,
    /// `<app>:<schedule>:<scheduled_for RFC 3339>` — the job-engine idempotency key.
    pub deduplication_key: String,
    /// Which schedule entry produced it, for the queue/budget the job engine needs.
    pub slot_ref: SlotRef,
}

/// The queue a [`SlotRef::new`] slot runs on: the manifest default.
pub const DEFAULT_QUEUE: &str = "default";

/// The time budget of a [`SlotRef::new`] slot: the manifest default, 600 000 ms.
pub const DEFAULT_BUDGET_MS: u64 = 600_000;

/// The app and schedule a slot came from, copied from the manifest entry the slot was planned
/// from, with the queue, step and time budget the job engine needs to run it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct SlotRef {
    /// The app whose manifest declared the schedule (its deduplication prefix).
    pub app: String,
    /// The schedule's name inside the manifest.
    pub schedule: String,
    /// The queue the schedule runs on, by its manifest name (`default`, `short` or `long`).
    pub queue: String,
    /// The app step the schedule runs (the manifest's `step`); empty for a [`SlotRef::new`]
    /// reference, which names no step.
    pub step: String,
    /// The longest a run may take, in milliseconds (the manifest's `budget_ms`).
    pub budget_ms: u64,
}

impl SlotRef {
    /// A reference to one app's schedule with the manifest defaults: queue `default`, no step,
    /// a budget of 600 000 ms. Prefer [`SlotRef::from_schedule`], which copies the real values.
    pub fn new(app: impl Into<String>, schedule: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            schedule: schedule.into(),
            queue: DEFAULT_QUEUE.to_owned(),
            step: String::new(),
            budget_ms: DEFAULT_BUDGET_MS,
        }
    }

    /// A reference built from the manifest's `[[schedules]]` entry: its queue, step and budget.
    ///
    /// A budget too large for `u64` milliseconds saturates (it can only be longer than any run).
    pub fn from_schedule(app: impl Into<String>, schedule: &ScheduleSub) -> Self {
        Self::from_parts(
            app.into(),
            &schedule.name,
            schedule.queue,
            schedule.step.as_str(),
            schedule.budget,
        )
    }

    /// The same, from the [`Schedule`] the scheduler plans with (it carries the manifest entry's
    /// queue, step and budget, plus its zone). [`Scheduler::tick`](crate::Scheduler::tick) uses
    /// this one.
    pub fn from_planned(app: impl Into<String>, schedule: &Schedule) -> Self {
        Self::from_parts(
            app.into(),
            &schedule.name,
            schedule.queue,
            schedule.step.as_str(),
            schedule.budget,
        )
    }

    fn from_parts(app: String, name: &str, queue: Queue, step: &str, budget: Duration) -> Self {
        Self {
            app,
            schedule: name.to_owned(),
            queue: queue_name(queue).to_owned(),
            step: step.to_owned(),
            budget_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// The manifest name of a queue.
fn queue_name(queue: Queue) -> &'static str {
    match queue {
        Queue::Default => DEFAULT_QUEUE,
        Queue::Short => "short",
        Queue::Long => "long",
    }
}

impl RunSlot {
    /// Builds the slot and its deduplication key.
    ///
    /// The key joins the app, the schedule name and `scheduled_for` rendered as RFC 3339 with
    /// **`:` separators** (`app:schedule:2026-10-06T14:00:00Z`), as the brief fixes: the colons
    /// make the key readable in job-engine logs where the app and schedule are already
    /// colon-qualified.
    pub fn new(
        app: &str,
        schedule: &str,
        scheduled_for: Timestamp,
        origin: SlotOrigin,
        slot_ref: SlotRef,
    ) -> Self {
        let _ = origin;
        let key = format!("{app}:{schedule}:{}", rfc3339(scheduled_for));
        Self {
            schedule: schedule.to_owned(),
            scheduled_for,
            deduplication_key: key,
            slot_ref,
        }
    }

    /// The key again, for callers that only kept the slot.
    pub fn key(&self) -> &str {
        &self.deduplication_key
    }
}

/// RFC 3339 with second precision in UTC, the canonical rendering the key uses.
///
/// Second precision is deliberate: cron's smallest field is the second, so two firings of one
/// schedule never share a second and no information is lost; rendering sub-second digits would
/// only bloat every key.
pub fn rfc3339(at: Timestamp) -> String {
    // jiff's Display for Timestamp is RFC 3339; second precision via nanosecond truncation.
    let nanos = at.subsec_nanosecond();
    let trimmed = at - jiff::Span::new().nanoseconds(nanos);
    format!("{trimmed:.0}")
}
