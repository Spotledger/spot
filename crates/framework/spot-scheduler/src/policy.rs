//! [`MissedRunPolicy`]: what happens to the runs a schedule missed while nobody ticked.

use jiff::Timestamp;

use crate::schedule::Schedule;
use crate::slot::{RunSlot, SlotOrigin, SlotRef};

/// How far one `plan` call may look back: an upper bound so a schedule whose high-water mark
/// was lost (or a fresh deployment with no mark) cannot enumerate unbounded history. Callers
/// that legitimately need more call `plan` in a loop until it returns fewer slots than the
/// limit.
pub const PLAN_DUP_LIMIT: usize = 10_000;

/// What to do with the runs missed between the last handled firing and `now`.
///
/// The brief fixes the three policies and the slot shape; the values here carry the manifest's
/// semantics, not storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissedRunPolicy {
    /// Fires nothing: missed runs are skipped, the next firing is the first future one.
    Skip,
    /// Collapses any number of missed runs into one run, scheduled for the **last** missed
    /// firing (the freshest possible recovery point).
    RunOnce,
    /// Runs one slot per missed firing, up to `max`; when more than `max` were missed the
    /// oldest ones are skipped and the `max` most recent each run.
    CatchUp(u32),
}

impl MissedRunPolicy {
    /// The name the manifest uses for the policy (`skip`, `run_once`, `catch_up`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::RunOnce => "run_once",
            Self::CatchUp(_) => "catch_up",
        }
    }
}

/// Plans the runs of one schedule between `last` (exclusive) and `now` (inclusive) under
/// `policy`.
///
/// `schedule`'s firings in that window come from [`Schedule::due_between`] (bounded by
/// [`PLAN_DUP_LIMIT`]); a `None` `last` starts at [`PLAN_FROM`], the beginning of
/// time, which only a caller that wants all history should pass (`Scheduler::tick` never does:
/// a schedule with no mark starts just before `now`). `skip` returns an empty vec; `run_once` returns one slot for the last
/// firing (origin `Missed`); `catch_up` returns up to `max` slots, oldest-skipped (the first
/// kept slot origin `Missed` when any were dropped, the rest `CatchUp`). An empty window
/// (nothing missed) returns empty under every policy — the *next* firing is planned by the
/// next tick that reaches it, not by this one.
pub fn plan(
    schedule: &Schedule,
    app: &str,
    last: Option<Timestamp>,
    now: Timestamp,
    policy: MissedRunPolicy,
    slot_ref: SlotRef,
) -> Vec<RunSlot> {
    let from = last.unwrap_or(PLAN_FROM);
    let firings = schedule.due_between(from, now, PLAN_DUP_LIMIT);
    if firings.is_empty() {
        return Vec::new();
    }
    let schedule_name = slot_ref.schedule.clone();
    match policy {
        MissedRunPolicy::Skip => Vec::new(),
        MissedRunPolicy::RunOnce => {
            let at = firings[firings.len() - 1];
            vec![RunSlot::new(
                app,
                &schedule_name,
                at,
                SlotOrigin::Missed,
                slot_ref,
            )]
        }
        MissedRunPolicy::CatchUp(max) => {
            let max = usize::try_from(u64::from(max.max(1))).unwrap_or(PLAN_DUP_LIMIT);
            let max = max.min(PLAN_DUP_LIMIT);
            let kept_start = firings.len().saturating_sub(max);
            let skipped = kept_start;
            firings[kept_start..]
                .iter()
                .enumerate()
                .map(|(index, at)| {
                    let origin = if index == 0 && skipped > 0 {
                        SlotOrigin::Missed
                    } else {
                        SlotOrigin::CatchUp
                    };
                    RunSlot::new(app, &schedule_name, *at, origin, slot_ref.clone())
                })
                .collect()
        }
    }
}

/// Where a plan with no mark starts: the beginning of time (the `due_between` limit keeps the
/// enumeration finite, so it returns the first [`PLAN_DUP_LIMIT`] firings, not the recent ones).
pub const PLAN_FROM: Timestamp = jiff::Timestamp::MIN;
