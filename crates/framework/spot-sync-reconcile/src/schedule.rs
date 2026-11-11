//! The reconciliation cycle as a `spot-scheduler` schedule.
//!
//! ADR-23 fixes "daily reconciliation per branch". A daily cycle is exactly an ordinary
//! manifest `[[schedules]]` entry, so it inherits DST-correct firing, missed-run policies and
//! leader leases from `spot-scheduler` (WP-PLT-07) rather than growing its own timer. This
//! module turns a small [`CycleSettings`] into that [`Schedule`], daily at 02:00 UTC unless the
//! tenant says otherwise, and exposes the due firings for a tick.

use std::time::Duration;

use jiff::Timestamp;
use spot_app_api::StepName;
use spot_manifest::{Cron, Queue};
use spot_scheduler::{Schedule, ScheduleBuilder, TimeZone};

use crate::error::{ReconcileError, ReconcileResult};

/// The globally unique step the cycle runs as.
pub const CYCLE_STEP: &str = "spot_sync_reconcile:run_cycle";

/// The schedule's name.
pub const CYCLE_SCHEDULE: &str = "sync_reconcile";

/// The cron a tenant gets when it states nothing: 02:00 UTC every day.
pub const DEFAULT_CRON: &str = "0 0 2 * * *";

/// The zone a tenant gets when it states nothing.
pub const DEFAULT_ZONE: &str = "UTC";

/// The cycle's longest run, when a tenant states nothing: one hour.
pub const DEFAULT_BUDGET_SECS: u64 = 3_600;

/// A tenant's reconciliation-cycle settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleSettings {
    /// When the cycle fires: six cron fields, seconds first.
    pub cron: String,
    /// The IANA zone the cron fields are read in.
    pub zone: String,
    /// The longest the cycle may take.
    pub budget_secs: u64,
    /// Whether the host registers the cycle at all.
    pub enabled: bool,
}

impl Default for CycleSettings {
    fn default() -> Self {
        Self {
            cron: DEFAULT_CRON.to_owned(),
            zone: DEFAULT_ZONE.to_owned(),
            budget_secs: DEFAULT_BUDGET_SECS,
            enabled: true,
        }
    }
}

impl CycleSettings {
    /// Checks the cron expression and the zone, without building the schedule.
    pub fn validate(&self) -> ReconcileResult<()> {
        parse_cron(&self.cron)?;
        parse_zone(&self.zone)?;
        Ok(())
    }
}

/// Builds the cycle's `spot-scheduler` schedule, or `None` when the tenant disabled it.
pub fn cycle_schedule(settings: &CycleSettings) -> ReconcileResult<Option<Schedule>> {
    if !settings.enabled {
        return Ok(None);
    }
    let cron = parse_cron(&settings.cron)?;
    let zone = parse_zone(&settings.zone)?;
    let step = StepName::new(CYCLE_STEP).map_err(|error| ReconcileError::InvalidSchedule {
        reason: format!("the cycle step name is invalid: {error}"),
    })?;
    let schedule = ScheduleBuilder::new(CYCLE_SCHEDULE)
        .cron(cron)
        .step(step)
        .queue(Queue::Long)
        .budget(Duration::from_secs(settings.budget_secs))
        .zone(zone)
        .build()
        .map_err(|error| ReconcileError::InvalidSchedule {
            reason: format!("the cycle schedule is invalid: {error}"),
        })?;
    Ok(Some(schedule))
}

/// The cycle firings in the half-open window `(from, to]`, earliest first, at most `limit`.
pub fn due_cycles(
    schedule: &Schedule,
    from: Timestamp,
    to: Timestamp,
    limit: usize,
) -> Vec<Timestamp> {
    schedule.due_between(from, to, limit)
}

fn parse_cron(raw: &str) -> ReconcileResult<Cron> {
    Cron::parse(raw).map_err(|error| ReconcileError::InvalidSchedule {
        reason: format!("cron {raw:?}: {error}"),
    })
}

fn parse_zone(raw: &str) -> ReconcileResult<TimeZone> {
    TimeZone::parse(raw).map_err(|error| ReconcileError::InvalidSchedule {
        reason: format!("zone {raw:?}: {error}"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_default_cycle_is_daily() {
        let settings = CycleSettings::default();
        assert_eq!(settings.cron, "0 0 2 * * *");
        let schedule = cycle_schedule(&settings).unwrap().unwrap();
        let from: Timestamp = "2026-03-01T00:00:00Z".parse().unwrap();
        let next = schedule.next_after(from).unwrap();
        assert_eq!(next.to_string(), "2026-03-01T02:00:00Z");
        let following = schedule.next_after(next).unwrap();
        assert_eq!(following.to_string(), "2026-03-02T02:00:00Z");
    }

    #[test]
    fn a_disabled_cycle_builds_no_schedule() {
        let settings = CycleSettings {
            enabled: false,
            ..CycleSettings::default()
        };
        assert!(cycle_schedule(&settings).unwrap().is_none());
    }

    #[test]
    fn a_bad_cron_or_zone_is_a_typed_error() {
        let bad_cron = CycleSettings {
            cron: "not a cron".into(),
            ..CycleSettings::default()
        };
        assert_eq!(
            cycle_schedule(&bad_cron).unwrap_err().code(),
            "sync_reconcile.invalid_schedule"
        );
        let bad_zone = CycleSettings {
            zone: "Mars/Olympus".into(),
            ..CycleSettings::default()
        };
        assert_eq!(
            cycle_schedule(&bad_zone).unwrap_err().code(),
            "sync_reconcile.invalid_schedule"
        );
    }

    #[test]
    fn due_cycles_lists_every_firing_in_the_window() {
        let schedule = cycle_schedule(&CycleSettings::default()).unwrap().unwrap();
        let from: Timestamp = "2026-03-01T00:00:00Z".parse().unwrap();
        let to: Timestamp = "2026-03-04T00:00:00Z".parse().unwrap();
        let due = due_cycles(&schedule, from, to, 10);
        assert_eq!(due.len(), 3);
        assert_eq!(due[0].to_string(), "2026-03-01T02:00:00Z");
    }
}
