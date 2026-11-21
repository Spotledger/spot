//! DST and cron-union behaviour, table-driven: the pinned cases the brief requires.

use jiff::Timestamp;
use spot_app_api::StepName;
use spot_manifest::{Cron, Queue};
use spot_scheduler::{Schedule, TimeZone, day_matches};

/// UTC instant from its RFC 3339 text.
fn ts(text: &str) -> Timestamp {
    text.parse()
        .unwrap_or_else(|_| panic!("valid RFC 3339 timestamp: {text}"))
}

/// One DST table case: the schedule `cron` in `zone`, searched after `from`, fires `expect`.
struct Case {
    /// The cron expression, six fields, seconds first.
    cron: &'static str,
    /// The IANA zone.
    zone: &'static str,
    /// The instant the search starts after.
    from: &'static str,
    /// The expected next firing(s): one per row.
    expect: &'static str,
    /// Why the case is in the table.
    why: &'static str,
}

/// The table: spring-forward gaps, fall-back folds, leap days, month ends and the union rule.
///
/// 12+ rows; every row pins an instant in UTC so a tzdb update that changes transitions fails
/// the case loudly rather than silently.
const CASES: [Case; 16] = [
    // America/New_York: spring forward 2026-03-08 02:00 EST -> 03:00 EDT; fall back 2026-11-01
    // 02:00 EDT -> 01:00 EST (US rules since 2007: second Sunday in March, first in November).
    Case {
        cron: "0 30 2 * * *",
        zone: "America/New_York",
        from: "2026-03-08T00:00:00Z",
        // 02:30 does not exist; the first valid instant after the gap is 03:30 EDT.
        expect: "2026-03-08T07:30:00Z",
        why: "spring-forward gap: 02:30 fires once, at 03:30 local",
    },
    Case {
        cron: "0 30 1 * * *",
        zone: "America/New_York",
        from: "2026-11-01T00:00:00Z",
        // Fold: 01:30 happens twice; the first occurrence is EDT (05:30Z).
        expect: "2026-11-01T05:30:00Z",
        why: "fall-back fold: 01:30 fires once, at the first occurrence",
    },
    Case {
        cron: "0 0 2 * * *",
        zone: "America/New_York",
        from: "2026-03-07T12:00:00Z",
        // The day before the change is untouched: plain EST.
        expect: "2026-03-08T07:00:00Z",
        why: "2:00 EST on the change day is inside the gap start",
    },
    Case {
        cron: "0 0 3 * * *",
        zone: "America/New_York",
        from: "2026-03-08T00:00:00Z",
        // 03:00 EDT is the first instant after the gap (07:00Z): it exists, so it fires there.
        expect: "2026-03-08T07:00:00Z",
        why: "03:00 EDT exists right after the gap",
    },
    Case {
        cron: "0 0 1 * * *",
        zone: "America/New_York",
        from: "2026-11-01T06:00:00Z",
        // The second 01:00 (EST, 06:00Z) is never fired again: past it already.
        expect: "2026-11-02T06:00:00Z",
        why: "fall-back: after the first occurrence the next day fires",
    },
    // Asia/Karachi: no DST; a control case.
    Case {
        cron: "0 30 2 * * *",
        zone: "Asia/Karachi",
        from: "2026-03-08T00:00:00Z",
        // 02:30 PKT on the 8th is 21:30Z on the 7th, before `from`; the next one is the 9th.
        expect: "2026-03-08T21:30:00Z",
        why: "no-DST zone: the next 02:30 PKT is 21:30Z the same UTC day",
    },
    // Europe/London: spring forward 2026-03-29 01:00 GMT -> 02:00 BST.
    Case {
        cron: "0 30 1 * * *",
        zone: "Europe/London",
        from: "2026-03-29T00:00:00Z",
        // 01:30 GMT exists right before the 01:00 gap.
        expect: "2026-03-29T01:30:00Z",
        why: "London: 01:30 GMT on the change day still exists",
    },
    Case {
        cron: "0 0 2 * * *",
        zone: "Europe/London",
        from: "2026-03-29T00:00:00Z",
        // The gap is 01:00-02:00 local; 02:00 BST exists and is 01:00Z.
        expect: "2026-03-29T01:00:00Z",
        why: "London: 02:00 BST is the first instant after the gap",
    },
    // Australia/Lord_Howe: a 30-minute DST step (both directions).
    Case {
        cron: "0 30 2 * * *",
        zone: "Australia/Lord_Howe",
        from: "2026-10-03T00:00:00Z",
        expect: "2026-10-03T15:30:00Z",
        why: "Lord Howe spring forward: 02:30 -> 03:00, 02:30 fires at 03:00 (+30m gap)",
    },
    // Leap day: every four years only; plus Feb-29 in a leap year.
    Case {
        cron: "0 0 0 29 2 *",
        zone: "UTC",
        from: "2026-03-01T00:00:00Z",
        expect: "2028-02-29T00:00:00Z",
        why: "leap day: the next Feb 29 is 2028",
    },
    Case {
        cron: "0 0 0 29 2 *",
        zone: "UTC",
        from: "2028-02-29T00:00:00Z",
        expect: "2032-02-29T00:00:00Z",
        why: "leap day chained from within a Feb 29",
    },
    // Month end: the 31st of months that have one.
    Case {
        cron: "0 0 0 31 * *",
        zone: "UTC",
        from: "2026-01-31T00:00:00Z",
        expect: "2026-03-31T00:00:00Z",
        why: "month end: Feb and Apr have no 31st; next is Mar 31",
    },
    // DOM 29 and DOW Friday (5), both restricted: union. The next February is 2027, whose first
    // Friday is the 5th; no Feb 29 is needed.
    Case {
        cron: "0 0 0 29 2 5",
        zone: "UTC",
        from: "2026-03-01T00:00:00Z",
        expect: "2027-02-05T00:00:00Z",
        why: "union rule: a Friday in February fires without a leap day",
    },
    // Union rule: DOM 1 and DOW Monday (1); 2026-02-01 is a Sunday, 2026-03-02 a Monday.
    Case {
        cron: "0 0 0 1 * 1",
        zone: "UTC",
        from: "2026-01-31T00:00:00Z",
        // Feb 1 fires (DOM) although it is a Sunday; a Monday-and-1st intersection would not.
        expect: "2026-02-01T00:00:00Z",
        why: "union rule: DOM 1 or Monday - Feb 1 is DOM",
    },
    Case {
        cron: "0 0 0 1 3 1",
        zone: "UTC",
        from: "2026-02-20T00:00:00Z",
        // March 2026: DOM=1 is a Sunday and still fires (union); Mondays follow: 2, 9, ...
        expect: "2026-03-01T00:00:00Z",
        why: "union rule: DOM 1 is a Sunday and fires anyway",
    },
    // DOW-only control: 5 is Friday; 2026-10-01 is a Thursday, so the next day at noon.
    Case {
        cron: "0 0 12 * * 5",
        zone: "UTC",
        from: "2026-10-01T00:00:00Z",
        expect: "2026-10-02T12:00:00Z",
        why: "DOW-only: 5 is Friday, first after Oct 1",
    },
];

/// The next firing of `case.cron` in `case.zone` after `case.from`, against the row's expect.
fn run_case(table_case: &Case) -> Result<(), String> {
    let zone = match TimeZone::parse(table_case.zone) {
        Ok(zone) => zone,
        Err(err) => return Err(err.to_string()),
    };
    let cron_expr = match Cron::parse(table_case.cron) {
        Ok(cron) => cron,
        Err(err) => return Err(format!("cron {:?}: {err}", table_case.cron)),
    };
    let step = match StepName::new("app:step") {
        Ok(step) => step,
        Err(err) => return Err(format!("step: {err}")),
    };
    let schedule = Schedule::build(
        "t".to_owned(),
        cron_expr,
        step,
        Queue::Default,
        std::time::Duration::from_millis(600_000),
        zone,
    )
    .map_err(|err| format!("schedule: {err}"))?;
    let got = schedule.next_after(ts(table_case.from));
    let expect = ts(table_case.expect);
    match got {
        Some(at) if at == expect => Ok(()),
        Some(at) => Err(format!(
            "{why}: cron {cron:?} in {zone_name} after {from}: expected {expect}, got {at}",
            why = table_case.why,
            cron = table_case.cron,
            zone_name = table_case.zone,
            from = table_case.from,
            expect = table_case.expect,
            at = at,
        )),
        None => Err(format!(
            "{why}: cron {cron:?} in {zone_name} after {from}: expected {expect}, got none",
            why = table_case.why,
            cron = table_case.cron,
            zone_name = table_case.zone,
            from = table_case.from,
            expect = table_case.expect,
        )),
    }
}

#[test]
fn dst_and_cron_table() {
    let mut failures = Vec::new();
    for table_case in &CASES {
        if let Err(err) = run_case(table_case) {
            failures.push(err);
        }
    }
    assert!(
        failures.is_empty(),
        "DST table failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn dst_table_has_enough_cases() {
    const MINIMUM: usize = 12;
    assert!(CASES.len() >= MINIMUM, "the brief needs at least 12 cases");
}

/// The fall-back run-once rule end to end: on the change day a 01:30 job appears exactly once
/// in a whole-day window even though the wall time happens twice: at its first occurrence.
#[test]
fn fall_back_runs_once_over_whole_day() {
    let schedule = at_0230_zone("America/New_York", "0 30 1 * * *");
    let day = (ts("2026-11-01T00:00:00Z"), ts("2026-11-02T00:00:00Z"));
    let fires = schedule.due_between(day.0, day.1, 100);
    // 01:30 happens twice; only the first occurrence (EDT, 05:30Z) fires.
    assert_eq!(fires, vec![ts("2026-11-01T05:30:00Z")]);
    // Across two days the next day's 01:30 EST (06:30Z) follows: one run a day, never two.
    let two = schedule.due_between(day.0, ts("2026-11-03T00:00:00Z"), 100);
    assert_eq!(
        two,
        vec![ts("2026-11-01T05:30:00Z"), ts("2026-11-02T06:30:00Z")]
    );
}

/// The spring-forward run-once-after-gap rule end to end: a 02:30 job on the change day fires
/// exactly once, at the first valid instant after the gap.
#[test]
fn spring_forward_runs_once_after_gap() {
    let schedule = at_0230_zone("America/New_York", "0 30 2 * * *");
    let day = (ts("2026-03-08T00:00:00Z"), ts("2026-03-09T00:00:00Z"));
    let fires = schedule.due_between(day.0, day.1, 100);
    assert_eq!(fires, vec![ts("2026-03-08T07:30:00Z")]);
}

/// A union-rule pair pinned through real schedules: "DOM 1 or Monday" over two months.
#[test]
fn union_rule_through_schedules() {
    // Both restricted: union (1st of month OR Mondays).
    let union = at_0230_zone("UTC", "0 0 0 1 * 1");
    let fires = union.due_between(ts("2026-02-25T00:00:00Z"), ts("2026-03-31T00:00:00Z"), 100);
    let expected = vec![
        ts("2026-03-01T00:00:00Z"), // Sunday: DOM restricted matches.
        ts("2026-03-02T00:00:00Z"), // Monday: DOW restricted matches.
        ts("2026-03-09T00:00:00Z"),
        ts("2026-03-16T00:00:00Z"),
        ts("2026-03-23T00:00:00Z"),
        ts("2026-03-30T00:00:00Z"),
    ];
    assert_eq!(fires, expected);
}

/// DOW-only (DOM = `*`): Mondays only, no union.
#[test]
fn dow_only_no_union() {
    let dow = at_0230_zone("UTC", "0 0 0 * * 1");
    let fires = dow.due_between(ts("2026-02-25T00:00:00Z"), ts("2026-03-10T00:00:00Z"), 100);
    assert_eq!(
        fires,
        vec![ts("2026-03-02T00:00:00Z"), ts("2026-03-09T00:00:00Z")]
    );
}

/// DOM-only (`dow = *`): the 1st only, no Mondays.
#[test]
fn dom_only_no_union() {
    let dom = at_0230_zone("UTC", "0 0 0 1 * *");
    let fires = dom.due_between(ts("2026-02-25T00:00:00Z"), ts("2026-04-02T00:00:00Z"), 100);
    assert_eq!(
        fires,
        vec![ts("2026-03-01T00:00:00Z"), ts("2026-04-01T00:00:00Z")]
    );
}

/// The pure truth table of `day_matches`, cron 0.12's union rule.
#[test]
fn union_truth_table() {
    // Both restricted: union.
    assert!(day_matches(true, true, true, false));
    assert!(day_matches(true, false, true, true));
    assert!(!day_matches(true, false, true, false));
    // One restricted: that field decides.
    assert!(day_matches(true, true, false, false));
    assert!(!day_matches(true, false, false, true));
    assert!(day_matches(false, true, true, true));
    assert!(!day_matches(false, true, true, false));
    // Neither: every day.
    assert!(day_matches(false, true, false, true));
}

/// A schedule helper for the tests above: named schedule from raw parts.
///
/// Panics name the failing cron so a grammar surprise is readable in test output.
pub fn zone_schedule(name: &str, zone_name: &str, cron_text: &str) -> Result<Schedule, String> {
    let cron_expr = Cron::parse(cron_text).map_err(|err| format!("cron {cron_text:?}: {err}"))?;
    let step = StepName::new("app:step").map_err(|err| format!("step: {err}"))?;
    let zone = TimeZone::parse(zone_name).map_err(|err| format!("zone {zone_name}: {err}"))?;
    Schedule::build(
        name.to_owned(),
        cron_expr,
        step,
        Queue::Default,
        std::time::Duration::from_millis(600_000),
        zone,
    )
    .map_err(|err| format!("schedule: {err}"))
}

/// A schedule helper for the tests above.
fn at_0230_zone(zone_name: &str, cron_text: &'static str) -> Schedule {
    zone_schedule("t", zone_name, cron_text).unwrap_or_else(|err| panic!("{err}"))
}

/// Day-of-week numbers are the usual ones (7 Sunday, 1 Monday), not the `cron` crate's (1 Sunday).
/// (`0` for Sunday never reaches here: `spot-manifest`'s validation, which asks the `cron` crate,
/// rejects it.)
#[test]
fn day_of_week_numbers_follow_the_manifest() {
    // 2026-10-01 is a Thursday: Sunday is the 4th, Monday the 5th, Friday the 2nd.
    for (cron, expect) in [
        ("0 0 0 * * 7", "2026-10-04T00:00:00Z"),
        ("0 0 0 * * 1", "2026-10-05T00:00:00Z"),
        ("0 0 0 * * 5", "2026-10-02T00:00:00Z"),
        ("0 0 0 * * 5-7", "2026-10-02T00:00:00Z"),
        ("0 0 0 * * 1-5", "2026-10-02T00:00:00Z"),
    ] {
        let schedule = at_0230_zone("UTC", cron);
        assert_eq!(
            schedule.next_after(ts("2026-10-01T00:00:00Z")),
            Some(ts(expect)),
            "{cron}"
        );
    }
}
