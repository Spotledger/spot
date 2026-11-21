//! The snapshot schedule and settings (WP-RTB-01.T5): the pure parts of the hosted service that
//! need no database. The image reader, the wait-for-the-journal rule and the retention plan are
//! proved against real PostgreSQL and plain lists in `spot-journal-snapshot-pg`'s own tests, and
//! the whole service against a real node in `l3-journal-sync`'s `snapshots.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::journal_snapshots::{Settings, daily_due};

const MINUTE: i64 = 60_000;
const DAY: i64 = 1_440 * MINUTE;

/// 2026-01-10 00:00:00 UTC in milliseconds (a day boundary: 20_463 days after the epoch).
const MIDNIGHT: i64 = 20_463 * DAY;

fn at(hours: i64, minutes: i64) -> i64 {
    MIDNIGHT + hours * 60 * MINUTE + minutes * MINUTE
}

#[test]
fn the_daily_snapshot_is_due_at_the_configured_time_and_once_per_day() {
    let due = 2 * 60 + 30;
    // Before 02:30 nothing is due, whatever was taken yesterday.
    assert!(!daily_due(at(2, 29), 0, due, Some(at(2, 30) - DAY)));
    // At 02:30 it is due when nothing was taken since this day's 02:30.
    assert!(daily_due(at(2, 30), 0, due, Some(at(2, 30) - DAY)));
    assert!(daily_due(at(2, 30), 0, due, None));
    // Taken at 02:31 today: not due again until tomorrow.
    assert!(!daily_due(at(14, 0), 0, due, Some(at(2, 31))));
    assert!(daily_due(at(2, 30) + DAY, 0, due, Some(at(2, 31))));
    // A snapshot taken by a migration at 01:00 does not satisfy the day's 02:30.
    assert!(daily_due(at(3, 0), 0, due, Some(at(1, 0))));
}

#[test]
fn the_site_offset_moves_the_quiet_hour() {
    let due = 2 * 60 + 30;
    // Site at UTC+5 (300 minutes): 02:30 local is 21:30 UTC the day before.
    let local_0230_in_utc =
        at(0, 0) - 2 * 60 * MINUTE - 30 * MINUTE + 5 * 60 * MINUTE - 5 * 60 * MINUTE;
    let _ = local_0230_in_utc;
    let utc_2130 = at(21, 30) - DAY;
    assert!(!daily_due(utc_2130 - MINUTE, 300, due, None));
    assert!(daily_due(utc_2130, 300, due, None));
}

#[test]
fn the_daily_time_is_validated() {
    let ok = Settings::default();
    assert_eq!(ok.at_minutes().unwrap(), 150);
    for bad in ["25:00", "02:60", "0230", "", "aa:bb"] {
        let settings = Settings {
            at: bad.to_owned(),
            ..Settings::default()
        };
        assert!(settings.at_minutes().is_err(), "{bad}");
    }
}

#[test]
fn the_defaults_are_the_briefs() {
    let settings = Settings::default();
    assert_eq!((settings.keep_daily, settings.keep_weekly), (7, 4));
    assert_eq!(settings.at, "02:30");
    assert!(settings.enabled);
}
