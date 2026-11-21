//! [`Schedule`]: a manifest [`ScheduleSub`](spot_manifest::ScheduleSub) plus an IANA zone, with
//! DST-correct firing times.
//!
//! The cron grammar is the `cron` crate's six-field one (seconds first), the same grammar
//! `spot-manifest` validates with, so every manifest schedule parses here unchanged. Day
//! matching follows cron 0.12's day-of-month/day-of-week union rule (see
//! [`crate::cron_union`]).
//!
//! DST: `cron` enumerates local wall times and [`crate::wall`] gives each one instant. A wall
//! time inside a spring-forward gap fires **once**, at the first valid instant after the gap
//! (the same wall reading under the offset in force before the gap); a wall time repeated by a
//! fall-back fires once, at its **first** occurrence, because [`Schedule::next_after`] searches
//! strictly after the instant it returned. That is the declared policy this crate implements.
//!
//! Leap days (`0 0 0 29 2 *`) and month ends (`0 0 0 31 * *`) are the cron crate's own
//! semantics; the union rule and the day-of-week numbering are in [`crate::cron_union`]. The
//! tests pin all of this with table cases.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use spot_app_api::StepName;
use spot_manifest::{Cron, Queue, ScheduleSub};

use crate::cron_union;
use crate::error::{Error, INVALID_CRON, INVALID_ZONE, Result};
use crate::wall;

/// How far [`Schedule::next_after`] may search for a next firing before giving up, in days.
///
/// Eight years: a Feb-29 expression waits four years for the next leap day, and eight across
/// a skipped century leap year (2096 to 2104).
pub const MAX_CRON_LOOKAHEAD_DAYS: i64 = 2930;

/// An IANA time-zone name with its compiled [`jiff::tz::TimeZone`].
///
/// Cloned cheaply (`Arc`); equality is by IANA name, so two schedules with the same name and
/// rule but a different zone name count as different (the name is what a manifest states).
#[derive(Debug, Clone)]
pub struct TimeZone {
    name: Arc<str>,
    tz: jiff::tz::TimeZone,
}

impl TimeZone {
    /// Parses an IANA zone name (`Asia/Karachi`) and loads its rules from the compiled tzdb.
    ///
    /// POSIX TZ strings are not IANA names and are refused: a manifest states a zone.
    pub fn parse(name: &str) -> Result<Self> {
        let tz = jiff::tz::TimeZone::get(name).map_err(|err| {
            Error::new(
                INVALID_ZONE,
                format!("time zone {name:?} is not a known IANA zone: {err}"),
            )
        })?;
        Ok(Self {
            name: name.into(),
            tz,
        })
    }

    /// The IANA name as written.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The compiled rules, for callers that need offsets directly.
    pub fn rules(&self) -> &jiff::tz::TimeZone {
        &self.tz
    }
}

impl PartialEq for TimeZone {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for TimeZone {}

/// One manifest `[[schedules]]` entry plus its zone: when it fires.
///
/// Built with [`Schedule::new`] (from a [`ScheduleSub`]) or [`Schedule::build`] (from raw
/// parts, for hosts that assemble schedules in code).
#[derive(Clone, Debug)]
pub struct Schedule {
    /// The schedule's name, unique per app manifest.
    pub name: String,
    /// The cron expression, six fields, seconds first.
    pub cron: Cron,
    /// The app's step that runs.
    pub step: StepName,
    /// The queue it runs on.
    pub queue: Queue,
    /// The longest it may take.
    pub budget: std::time::Duration,
    /// Where the wall-clock fields are read.
    pub zone: TimeZone,
    /// The parsed cron programs: one, or two when both day fields are restricted (union rule,
    /// see [`crate::cron_union`]).
    programs: Vec<cron::Schedule>,
}

impl Schedule {
    /// Builds a schedule from a manifest entry plus a zone.
    pub fn new(sub: &ScheduleSub, zone: TimeZone) -> Result<Self> {
        Self::build(
            sub.name.clone(),
            sub.cron.clone(),
            sub.step.clone(),
            sub.queue,
            sub.budget,
            zone,
        )
    }

    /// Builds a schedule from raw parts.
    pub fn build(
        name: String,
        cron_expr: Cron,
        step: StepName,
        queue: Queue,
        budget: std::time::Duration,
        zone: TimeZone,
    ) -> Result<Self> {
        let programs = cron_union::programs(cron_expr.as_str()).map_err(|err| {
            Error::new(
                INVALID_CRON,
                format!("cron {:?}: {err}", cron_expr.as_str()),
            )
        })?;
        Ok(Self {
            name,
            cron: cron_expr,
            step,
            queue,
            budget,
            zone,
            programs,
        })
    }

    /// The next instant strictly after `after` when this schedule fires in its zone.
    ///
    /// DST-correct (see the module docs); fails only when no firing exists within
    /// [`MAX_CRON_LOOKAHEAD_DAYS`], which in practice only a leap-day expression that starts in
    /// the first days after Feb 29 of a leap year could approach.
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        self.next_after_within(after, MAX_CRON_LOOKAHEAD_DAYS)
    }

    /// Like [`Schedule::next_after`], but bounded to `days` of search — the hook the
    /// simulation uses to keep a pathological expression from spinning.
    pub fn next_after_within(&self, after: Timestamp, days: i64) -> Option<Timestamp> {
        // `Timestamp` arithmetic in jiff takes hour-or-smaller units; the day bound is hours.
        let search_end = after + jiff::Span::new().hours(days * 24);
        self.programs
            .iter()
            .filter_map(|program| wall::next_after(program, &self.zone.tz, after, search_end))
            .min()
    }

    /// Every firing in the half-open window `(from, to]`, earliest first, at most `limit`.
    ///
    /// Callers pass `from` = the last handled time (or the high-water mark) and `to` = now; the
    /// returned instants are the runs due in between. The collect stops at `limit`; callers
    /// that need the rest re-tick.
    pub fn due_between(&self, from: Timestamp, to: Timestamp, limit: usize) -> Vec<Timestamp> {
        let mut out = Vec::new();
        let mut cursor = from;
        while out.len() < limit {
            let Some(next) = self.next_after(cursor) else {
                break;
            };
            if next > to {
                break;
            }
            out.push(next);
            cursor = next;
        }
        out
    }
}

/// Assembles a [`Schedule`] piece by piece, for hosts that build schedules in code.
#[derive(Debug, Clone)]
pub struct ScheduleBuilder {
    name: String,
    cron: Option<Cron>,
    step: Option<StepName>,
    queue: Option<Queue>,
    budget: Option<std::time::Duration>,
    zone: Option<TimeZone>,
}

impl ScheduleBuilder {
    /// An empty builder for schedule `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cron: None,
            step: None,
            queue: None,
            budget: None,
            zone: None,
        }
    }

    /// The cron expression (required).
    pub fn cron(mut self, cron: Cron) -> Self {
        self.cron = Some(cron);
        self
    }

    /// The step that runs (required).
    pub fn step(mut self, step: StepName) -> Self {
        self.step = Some(step);
        self
    }

    /// The queue (optional; default `Queue::Default`).
    pub fn queue(mut self, queue: Queue) -> Self {
        self.queue = Some(queue);
        self
    }

    /// The budget (optional; default 600 000 ms).
    pub fn budget(mut self, budget: std::time::Duration) -> Self {
        self.budget = Some(budget);
        self
    }

    /// The zone (required).
    pub fn zone(mut self, zone: TimeZone) -> Self {
        self.zone = Some(zone);
        self
    }

    /// Builds; fails with `scheduler.invalid_zone` when a required part is missing or the cron
    /// does not parse.
    pub fn build(self) -> Result<Schedule> {
        let cron = self
            .cron
            .ok_or_else(|| Error::new(INVALID_ZONE, "schedule has no cron expression"))?;
        let step = self
            .step
            .ok_or_else(|| Error::new(INVALID_ZONE, "schedule has no step"))?;
        let zone = self
            .zone
            .ok_or_else(|| Error::new(INVALID_ZONE, "schedule has no time zone"))?;
        Schedule::build(
            self.name,
            cron,
            step,
            self.queue.unwrap_or(Queue::Default),
            self.budget.unwrap_or(Duration::from_millis(
                spot_manifest::SCHEDULE_BUDGET_DEFAULT_MS,
            )),
            zone,
        )
    }
}
