//! Backup schedules: the `spot-scheduler` schedule plus the job-engine handover.
//!
//! A [`BackupSchedule`] wraps one [`spot_scheduler::Schedule`]. Each due firing becomes
//! a [`spot_scheduler::RunSlot`]; [`enqueue_due`] hands those slots to `spot-jobs` with
//! the slot's deduplication key as the job's idempotency key, so a failover or a second
//! tick never runs one backup twice.

use spot_jobs::model::InsertResult;
use spot_jobs::{JobStore, NewJob};
use spot_scheduler::{RunSlot, Schedule, SlotOrigin, SlotRef};
use time::OffsetDateTime;

use crate::error::{BackupError, Result};

/// The job name every fired backup slot uses (`app:job`, as `spot-jobs` requires).
pub const BACKUP_JOB_NAME: &str = "spot_backup:backup";

/// The app prefix in a slot's deduplication key.
pub const BACKUP_APP: &str = "spot_backup";

/// A backup schedule and the queue/budget source it wraps.
#[derive(Debug, Clone)]
pub struct BackupSchedule {
    schedule: Schedule,
}

impl BackupSchedule {
    /// Wraps an existing scheduler schedule.
    pub fn new(schedule: Schedule) -> BackupSchedule {
        BackupSchedule { schedule }
    }

    /// The underlying schedule.
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// The job queue this schedule runs on.
    pub fn queue(&self) -> &'static str {
        match self.schedule.queue {
            spot_manifest::Queue::Default => "default",
            spot_manifest::Queue::Short => "short",
            spot_manifest::Queue::Long => "long",
        }
    }

    /// The budget in seconds, rounded up to at least one.
    pub fn budget_secs(&self) -> u64 {
        let millis = self.schedule.budget.as_millis() as u64;
        millis.saturating_add(999) / 1000
    }

    /// Firings in `(from, to]`, at most `limit`, as job slots.
    pub fn due_slots(
        &self,
        from: jiff::Timestamp,
        to: jiff::Timestamp,
        limit: usize,
    ) -> Vec<RunSlot> {
        self.schedule
            .due_between(from, to, limit)
            .into_iter()
            .map(|at| {
                RunSlot::new(
                    BACKUP_APP,
                    &self.schedule.name,
                    at,
                    SlotOrigin::Due,
                    SlotRef::new(BACKUP_APP, &self.schedule.name),
                )
            })
            .collect()
    }
}

/// Converts a slot's UTC instant to the `time` type the job engine uses.
///
/// # Errors
/// [`BackupError::io`] when the instant is outside `time`'s range (not reachable for a
/// cron firing in this century).
pub fn slot_instant(slot: &RunSlot) -> Result<OffsetDateTime> {
    let seconds = slot.scheduled_for.as_second();
    let nanos = slot.scheduled_for.subsec_nanosecond().max(0);
    let value = i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    OffsetDateTime::from_unix_timestamp_nanos(value)
        .map_err(|error| BackupError::io(format!("the slot instant is out of range: {error}")))
}

/// Enqueues every slot as one idempotent backup job.
///
/// # Errors
/// [`BackupError`] when the job store fails or a slot's instant is out of range.
pub async fn enqueue_due(
    store: &dyn JobStore,
    site: spot_core::SiteId,
    tenant: &str,
    schedule: &BackupSchedule,
    slots: &[RunSlot],
) -> Result<Vec<InsertResult>> {
    let mut results = Vec::with_capacity(slots.len());
    for slot in slots {
        let args = serde_json::json!({
            "tenant": tenant,
            "schedule": schedule.schedule.name,
            "scheduled_for": slot.scheduled_for.to_string(),
            "deduplication_key": slot.deduplication_key,
        });
        let job = NewJob::new(
            BACKUP_JOB_NAME.to_owned(),
            args,
            schedule.queue().to_owned(),
            site.clone(),
            slot_instant(slot)?,
        )
        .with_idempotency_key(slot.deduplication_key.clone());
        results.push(store.insert_if_new(job).await?);
    }
    Ok(results)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_jobs::InMemoryJobStore;
    use spot_scheduler::{ScheduleBuilder, TimeZone};
    use std::str::FromStr;
    use std::sync::Arc;

    fn schedule() -> BackupSchedule {
        let cron = spot_manifest::Cron::from_str("0 0 2 * * *").expect("cron");
        let step = spot_app_api::StepName::new("spot_backup:backup").expect("step");
        let schedule = ScheduleBuilder::new("nightly")
            .cron(cron)
            .step(step)
            .queue(spot_manifest::Queue::Long)
            .zone(TimeZone::parse("UTC").expect("zone"))
            .build()
            .expect("schedule");
        BackupSchedule::new(schedule)
    }

    fn now() -> jiff::Timestamp {
        "2026-10-19T00:00:00Z".parse().expect("instant")
    }

    #[test]
    fn a_daily_schedule_fires_once_per_day() {
        let schedule = schedule();
        let from = now() - jiff::Span::new().hours(48);
        let to = now() + jiff::Span::new().hours(48);
        let slots = schedule.due_slots(from, to, 10);
        assert_eq!(slots.len(), 4);
        assert_eq!(slots[0].schedule, "nightly");
        assert!(
            slots[0]
                .deduplication_key
                .starts_with("spot_backup:nightly:")
        );
        // Keys are stable across a re-plan.
        assert_eq!(
            slots[0].deduplication_key,
            schedule.due_slots(from, to, 10)[0].deduplication_key
        );
    }

    #[tokio::test]
    async fn an_enqueued_slot_is_deduplicated() {
        let store = Arc::new(InMemoryJobStore::with_clock(Arc::new(|| {
            OffsetDateTime::from_unix_timestamp(1_792_368_000).expect("now")
        })));
        let site = spot_core::SiteId::new("acme").expect("site");
        let schedule = schedule();
        let from = now();
        let to = now() + jiff::Span::new().hours(3);
        let slots = schedule.due_slots(from, to, 10);
        let first = enqueue_due(store.as_ref(), site.clone(), "acme", &schedule, &slots)
            .await
            .expect("first");
        assert!(matches!(first[0], InsertResult::Inserted(_)));
        let second = enqueue_due(store.as_ref(), site, "acme", &schedule, &slots)
            .await
            .expect("second");
        assert!(matches!(second[0], InsertResult::Duplicate(_)));
    }
}
