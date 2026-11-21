//! The recovery time objective: the tier target is an argument, and a breach says by how much.

use crate::{RtoVerdict, rto_check};

/// The standard tier's placeholder target, four hours.
const FOUR_HOURS: u64 = 4 * 60 * 60 * 1000;

#[test]
fn a_faster_rebuild_is_within_the_target() {
    let verdict = rto_check(FOUR_HOURS - 1, FOUR_HOURS);
    assert_eq!(verdict, RtoVerdict::Within);
    assert!(verdict.is_within());
    assert_eq!(verdict.over_by_millis(), 0);
}

#[test]
fn a_rebuild_exactly_at_the_target_is_within_it() {
    assert_eq!(rto_check(FOUR_HOURS, FOUR_HOURS), RtoVerdict::Within);
}

#[test]
fn a_slower_rebuild_breaches_the_target_by_the_difference() {
    let verdict = rto_check(FOUR_HOURS + 1, FOUR_HOURS);
    assert_eq!(verdict, RtoVerdict::Breached { over_by_millis: 1 });
    assert!(!verdict.is_within());
    assert_eq!(verdict.over_by_millis(), 1);
}

#[test]
fn a_premium_target_is_a_different_argument() {
    const ONE_HOUR: u64 = 60 * 60 * 1000;
    // The same rebuild meets the premium target and misses nothing; the standard one passes too.
    assert_eq!(rto_check(ONE_HOUR - 1, ONE_HOUR), RtoVerdict::Within);
    assert_eq!(
        rto_check(ONE_HOUR + 1, ONE_HOUR),
        RtoVerdict::Breached { over_by_millis: 1 }
    );
    assert_eq!(rto_check(ONE_HOUR + 1, FOUR_HOURS), RtoVerdict::Within);
}

#[test]
fn extreme_durations_do_not_overflow() {
    assert_eq!(rto_check(0, 0), RtoVerdict::Within);
    assert_eq!(rto_check(1, 0), RtoVerdict::Breached { over_by_millis: 1 });
    assert_eq!(
        rto_check(u64::MAX, 0),
        RtoVerdict::Breached {
            over_by_millis: u64::MAX
        }
    );
    assert_eq!(rto_check(0, u64::MAX), RtoVerdict::Within);
    assert_eq!(rto_check(u64::MAX, u64::MAX), RtoVerdict::Within);
}
