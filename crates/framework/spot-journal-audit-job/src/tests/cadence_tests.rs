use spot_journal_store::TenantId;

use crate::{Cadence, DAY_MILLIS, TenantAuditPlan, due};

fn plan(cadence: Cadence, last: Option<i64>) -> TenantAuditPlan {
    TenantAuditPlan {
        tenant: TenantId::new("acme").unwrap(),
        cadence,
        last_run_millis: last,
    }
}

#[test]
fn cadence_table() {
    let hour = 3_600_000;
    let t0 = 5_000_000_000;
    let cases = [
        (Cadence::Weekly, None, t0, true),
        (Cadence::Daily, None, t0, true),
        (
            Cadence::Weekly,
            Some(t0),
            t0 + 6 * DAY_MILLIS + 23 * hour,
            false,
        ),
        (Cadence::Weekly, Some(t0), t0 + 7 * DAY_MILLIS, true),
        (Cadence::Weekly, Some(t0), t0 + 8 * DAY_MILLIS, true),
        (Cadence::Daily, Some(t0), t0 + 23 * hour, false),
        (Cadence::Daily, Some(t0), t0 + DAY_MILLIS, true),
        // A clock that went backwards is never due.
        (Cadence::Daily, Some(t0), t0 - DAY_MILLIS * 3, false),
        // No overflow at the extremes.
        (Cadence::Weekly, Some(i64::MIN), i64::MAX, true),
    ];
    for (cadence, last, now, expected) in cases {
        assert_eq!(
            due(&plan(cadence, last), now),
            expected,
            "{cadence:?} {last:?} {now}"
        );
    }
}
