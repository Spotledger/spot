//! Shared builders for the scheduler tests.
#![allow(dead_code)]

use std::sync::Arc;

use jiff::Timestamp;
use spot_app_api::StepName;
use spot_manifest::{Cron, Queue};
use spot_scheduler::{Schedule, TimeZone};

/// UTC instant from its RFC 3339 text.
pub fn ts(text: &str) -> Timestamp {
    text.parse()
        .unwrap_or_else(|_| panic!("valid RFC 3339 timestamp: {text}"))
}

/// A named schedule in `zone`.
pub fn schedule(name: &str, zone: &str, cron: &str) -> Arc<Schedule> {
    let cron = Cron::parse(cron).unwrap_or_else(|e| panic!("cron {cron:?}: {e}"));
    let step = StepName::new("app:step").unwrap_or_else(|e| panic!("step: {e}"));
    let zone = TimeZone::parse(zone).unwrap_or_else(|e| panic!("zone {zone}: {e}"));
    Arc::new(
        Schedule::build(
            name.to_owned(),
            cron,
            step,
            Queue::Default,
            std::time::Duration::from_secs(600),
            zone,
        )
        .unwrap_or_else(|e| panic!("schedule {name}: {e}")),
    )
}

/// A manual clock at the instant `at`.
pub fn clock_at(at: Timestamp) -> spot_core::ManualClock {
    spot_core::ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(at.as_second())
            .unwrap_or_else(|e| panic!("clock start: {e}")),
    )
}
