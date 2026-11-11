//! The checked clamp refuses unrepresentable stamps (WP-SYN-06.T4).

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::BTreeMap;

use proptest::prelude::*;
use spot_sync_freshness::{
    Bounds, FreshnessError, SyncStamp, clamp_offset, clamp_offset_checked, find_violations_clamped,
    reject_audit,
};

const DAY: i64 = 86_400_000;

fn bounds() -> Bounds {
    Bounds::new(2_000, 7 * DAY).expect("valid bounds")
}

fn stamp(wall: i64, offset: i64) -> SyncStamp {
    SyncStamp {
        node: "node-1".into(),
        stamped_at_millis: wall,
        hub_offset_ms: offset,
        applied_prefix: 5,
        class_horizons_confirmed: BTreeMap::new(),
        stale: vec![],
        metadata_version: 1,
        software_version: "1.0.0".into(),
        override_: None,
        warning_confirmed: None,
    }
}

fn in_window(stamped: i64, c_offset: i64, received: i64, b: &Bounds) -> bool {
    let t = i128::from(stamped) + i128::from(c_offset);
    t <= i128::from(received) + i128::from(b.tolerance_ms())
        && t >= i128::from(received) - i128::from(b.max_age_ms())
}

#[test]
fn extreme_corners_are_in_window_or_refused() {
    let b = bounds();
    for wall in [i64::MIN, i64::MAX] {
        for claim in [i64::MIN, -DAY, 0, DAY, i64::MAX] {
            for rec in [i64::MIN, 0, 1_000_000, i64::MAX] {
                if let Ok(c) = clamp_offset_checked(claim, wall, rec, &b) {
                    assert!(
                        in_window(wall, c.offset_ms, rec, &b),
                        "{wall} {claim} {rec}"
                    );
                } else {
                    assert_eq!(
                        clamp_offset_checked(claim, wall, rec, &b).unwrap_err(),
                        FreshnessError::StampUnrepresentable
                    );
                }
            }
        }
    }
}

#[test]
fn the_saturating_case_is_refused() {
    let b = bounds();
    // Unchecked saturates (documented behaviour); checked refuses.
    assert_eq!(clamp_offset(0, i64::MIN, i64::MAX, &b).offset_ms, i64::MAX);
    assert_eq!(
        clamp_offset_checked(0, i64::MIN, i64::MAX, &b).unwrap_err(),
        FreshnessError::StampUnrepresentable
    );
    // stamped + claimed overflows i64 even though the sum would be "clampable".
    assert_eq!(
        clamp_offset_checked(i64::MAX, i64::MAX, 0, &b).unwrap_err(),
        FreshnessError::StampUnrepresentable
    );
    assert_eq!(
        clamp_offset_checked(i64::MIN, i64::MIN, 0, &b).unwrap_err(),
        FreshnessError::StampUnrepresentable
    );
    assert_eq!(
        FreshnessError::StampUnrepresentable.code(),
        "sync_freshness.stamp_unrepresentable"
    );
}

#[test]
fn ordinary_stamps_match_the_unchecked_function() {
    let b = bounds();
    for (claim, wall, rec) in [
        (-3 * DAY, 99 * DAY, 100 * DAY),
        (5 * DAY, 1_000_000, 1_000_000),
        (500, 100_000, 100_500),
        (i64::MIN, 1_000_000, 1_000_000),
        (i64::MAX - 1_000_000, 1_000_000, 1_000_000),
        (0, i64::MAX, 0),
    ] {
        assert_eq!(
            clamp_offset_checked(claim, wall, rec, &b).expect("representable"),
            clamp_offset(claim, wall, rec, &b)
        );
    }
}

#[test]
fn hub_check_refuses_an_unrepresentable_stamp() {
    let b = bounds();
    let st = stamp(i64::MIN, 0);
    let e = find_violations_clamped("D", &st, i64::MAX, &b, &[]).unwrap_err();
    assert_eq!(e, FreshnessError::StampUnrepresentable);
}

#[test]
fn rejection_is_audited_without_payload() {
    let st = stamp(i64::MIN, 7);
    let n = reject_audit(&st, 1_234);
    assert_eq!(n.code, "sync_freshness.stamp_unrepresentable");
    assert_eq!(n.node, "node-1");
    assert_eq!(n.claimed_stamped_at_millis, i64::MIN);
    assert_eq!(n.claimed_offset_ms, 7);
    assert_eq!(n.hub_received_at_millis, 1_234);
    assert!(!n.reason.is_empty());
}

proptest! {
    #[test]
    fn ok_is_always_inside_the_window(
        wall in any::<i64>(),
        claim in prop_oneof![any::<i64>(), -3 * DAY..3 * DAY, Just(i64::MIN), Just(i64::MAX)],
        rec in prop_oneof![any::<i64>(), Just(0i64), Just(i64::MIN), Just(i64::MAX)],
    ) {
        let b = bounds();
        if let Ok(c) = clamp_offset_checked(claim, wall, rec, &b) {
            prop_assert!(in_window(wall, c.offset_ms, rec, &b));
            prop_assert!(wall.checked_add(c.offset_ms).is_some());
        }
    }
}
