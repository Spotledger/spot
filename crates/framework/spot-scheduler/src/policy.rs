//! [`MissedRunPolicy`]: what happens to the runs a schedule missed while nobody ticked.

use std::time::Duration;

use jiff::Timestamp;

use crate::schedule::Schedule;
use crate::slot::{RunSlot, SlotOrigin, SlotRef};

/// How far one `plan` call may look back: an upper bound so a schedule whose high-water mark
/// was lost (or a fresh deployment with no mark) cannot enumerate unbounded history. Callers
/// that legitimately need more call `plan` in a loop until it returns fewer slots than the
/// limit.
pub const PLAN_DUP_LIMIT: usize = 10_000;

/// How long after its scheduled instant a firing still counts as **on time**: a tick that
/// reaches a firing at most this late runs it under every policy, a firing that is older when a
/// tick first reaches it was *missed* (nobody was ticking). A default only: a host whose tick
/// interval is operator-configurable sets the grace from it with
/// [`Scheduler::with_skip_grace`](crate::Scheduler::with_skip_grace).
pub const DEFAULT_SKIP_GRACE: Duration = Duration::from_secs(60);

/// What to do with the runs missed between the last handled firing and `now`.
///
/// A firing in the planned window is **on time** when `now - firing <= grace` (see
/// [`DEFAULT_SKIP_GRACE`]) and **missed** otherwise. On-time firings run under every policy, one
/// slot each, in order, with origin [`SlotOrigin::Due`]; the policy decides only what becomes of
/// the missed ones:
///
/// - `skip` drops them;
/// - `run_once` collapses them into one `Missed` slot for the last missed firing;
/// - `catch_up(max)` runs up to `max` of them (the most recent), never limiting the on-time ones.
///
/// The brief fixes the three policies and the slot shape; the values here carry the manifest's
/// semantics, not storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissedRunPolicy {
    /// Missed runs are not replayed: only the firings that are on time run. A schedule under
    /// `skip` still fires every firing a tick reaches within the grace period.
    Skip,
    /// Collapses any number of missed runs into one run, scheduled for the **last** missed
    /// firing (the freshest possible recovery point), in addition to the on-time firings.
    RunOnce,
    /// Runs one slot per missed firing, up to `max`; when more than `max` were missed the
    /// oldest ones are skipped and the `max` most recent each run. The on-time firings run in
    /// addition and are not counted against `max`.
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
/// `policy`, with the [`DEFAULT_SKIP_GRACE`]. See [`plan_with_grace`].
pub fn plan(
    schedule: &Schedule,
    app: &str,
    last: Option<Timestamp>,
    now: Timestamp,
    policy: MissedRunPolicy,
    slot_ref: SlotRef,
) -> Vec<RunSlot> {
    plan_with_grace(
        schedule,
        app,
        last,
        now,
        policy,
        slot_ref,
        DEFAULT_SKIP_GRACE,
    )
}

/// Plans the runs of one schedule between `last` (exclusive) and `now` (inclusive) under
/// `policy`, where a firing `f` is on time when `now - f <= grace`.
///
/// `schedule`'s firings in that window come from [`Schedule::due_between`] (bounded by
/// [`PLAN_DUP_LIMIT`]); a `None` `last` starts at [`PLAN_FROM`], the beginning of
/// time, which only a caller that wants all history should pass (`Scheduler::tick` never does:
/// a schedule with no mark starts just before `now`).
///
/// - on-time firings: one slot each, in order, under **every** policy;
/// - `skip`: the missed firings (older than `grace`) are dropped;
/// - `run_once`: the missed firings collapse into one slot for the last of them (origin
///   `Missed`), planned before the on-time slots; nothing is collapsed into an on-time firing;
/// - `catch_up(max)`: at most `max` missed firings run, oldest dropped (the first kept one has
///   origin `Missed` when any were dropped, the rest `CatchUp`), then the on-time slots.
///
/// Slots come out in scheduled order and the keys are unchanged
/// (`<app>:<schedule>:<scheduled_for>`), so a re-plan after a failed handover is idempotent. An
/// empty window returns empty under every policy: the *next* firing is planned by the next tick
/// that reaches it, not by this one.
pub fn plan_with_grace(
    schedule: &Schedule,
    app: &str,
    last: Option<Timestamp>,
    now: Timestamp,
    policy: MissedRunPolicy,
    slot_ref: SlotRef,
    grace: Duration,
) -> Vec<RunSlot> {
    plan_origins_with_grace(schedule, app, last, now, policy, slot_ref, grace)
        .into_iter()
        .map(|(slot, _)| slot)
        .collect()
}

/// [`plan_with_grace`] with the [`SlotOrigin`] of each slot beside it (a [`RunSlot`] does not
/// carry its origin), for logs and for tests that check which slots are on time.
pub fn plan_origins_with_grace(
    schedule: &Schedule,
    app: &str,
    last: Option<Timestamp>,
    now: Timestamp,
    policy: MissedRunPolicy,
    slot_ref: SlotRef,
    grace: Duration,
) -> Vec<(RunSlot, SlotOrigin)> {
    let from = last.unwrap_or(PLAN_FROM);
    let firings = schedule.due_between(from, now, PLAN_DUP_LIMIT);
    if firings.is_empty() {
        return Vec::new();
    }
    let schedule_name = slot_ref.schedule.clone();
    // Firings are in ascending order, so the missed ones (the oldest) form a prefix.
    let split = firings.partition_point(|at| !is_on_time(*at, now, grace));
    let (missed, on_time) = firings.split_at(split);
    let make = |at: Timestamp, origin: SlotOrigin| {
        (
            RunSlot::new(app, &schedule_name, at, origin, slot_ref.clone()),
            origin,
        )
    };
    let mut out = Vec::new();
    if !missed.is_empty() {
        match policy {
            MissedRunPolicy::Skip => {}
            MissedRunPolicy::RunOnce => {
                out.push(make(missed[missed.len() - 1], SlotOrigin::Missed));
            }
            MissedRunPolicy::CatchUp(max) => {
                let max = usize::try_from(u64::from(max.max(1))).unwrap_or(PLAN_DUP_LIMIT);
                let max = max.min(PLAN_DUP_LIMIT);
                let kept_start = missed.len().saturating_sub(max);
                for (index, at) in missed[kept_start..].iter().enumerate() {
                    let origin = if index == 0 && kept_start > 0 {
                        SlotOrigin::Missed
                    } else {
                        SlotOrigin::CatchUp
                    };
                    out.push(make(*at, origin));
                }
            }
        }
    }
    out.extend(on_time.iter().map(|at| make(*at, SlotOrigin::Due)));
    out
}

/// Whether a tick at `now` reaches `firing` within `grace` (`now - firing <= grace`; the
/// boundary instant is on time). Nanosecond integers, no floats.
fn is_on_time(firing: Timestamp, now: Timestamp, grace: Duration) -> bool {
    let age = now.as_nanosecond() - firing.as_nanosecond();
    let grace = i128::try_from(grace.as_nanos()).unwrap_or(i128::MAX);
    age <= grace
}

/// Where a plan with no mark starts: the beginning of time (the `due_between` limit keeps the
/// enumeration finite, so it returns the first [`PLAN_DUP_LIMIT`] firings, not the recent ones).
pub const PLAN_FROM: Timestamp = jiff::Timestamp::MIN;
