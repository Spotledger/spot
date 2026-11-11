//! Behaviour of the hub-side offset clamp (WP-SYN-06.T2).

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::BTreeMap;

use proptest::prelude::*;
use spot_sync_freshness::{
    Bounds, ClampReason, FreshnessError, HubPostedAt, MandatoryEvent, SyncStamp, clamp_offset,
    find_violations, find_violations_clamped, stamp_audit,
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

fn mandatory(published_at: i64) -> MandatoryEvent {
    MandatoryEvent {
        position: 6,
        published_at,
        effective_from: None,
        record: "TaxRate/gst@2".into(),
    }
}

#[test]
fn honest_node_within_tolerance_is_unchanged() {
    let b = bounds();
    // Hub time of the stamp is 1 s after receipt (within the 2 s tolerance).
    let c = clamp_offset(500, 100_000, 100_500 - 1_000 + 1_000, &b);
    assert!(!c.was_clamped);
    assert_eq!(c.offset_ms, 500);
    assert_eq!(c.reason, None);
    assert!(stamp_audit(&stamp(100_000, 500), &c).is_none());
}

#[test]
fn three_days_into_the_past_is_clamped_and_the_violation_is_found() {
    let b = Bounds::new(2_000, DAY).expect("valid");
    let received = 10 * DAY;
    let wall = received - 1_000;
    // The rule was published two days ago, long before the node stamped.
    let ev = mandatory(received - 2 * DAY);
    // Hostile claim: moves the stamp 3 days back, before the event, to escape the finding.
    let claim = -3 * DAY;
    let st = stamp(wall, claim);
    assert!(
        find_violations(
            "D",
            &st,
            HubPostedAt::hub_received(received),
            std::slice::from_ref(&ev)
        )
        .is_empty()
    );
    let c = clamp_offset(claim, wall, received, &b);
    assert!(c.was_clamped);
    assert_eq!(c.reason, Some(ClampReason::TooOld));
    assert_eq!(wall + c.offset_ms, received - DAY);
    let f = find_violations_clamped("D", &st, received, &b, std::slice::from_ref(&ev))
        .expect("representable");
    assert_eq!(f.len(), 1);
    assert!(f[0].clamped);
    assert_eq!(f[0].effective_stamped_at_millis, received - DAY);
}

#[test]
fn future_claim_is_clamped_to_receipt_plus_tolerance() {
    let b = bounds();
    let c = clamp_offset(5 * DAY, 1_000_000, 1_000_000, &b);
    assert!(c.was_clamped);
    assert_eq!(c.reason, Some(ClampReason::InFuture));
    assert_eq!(1_000_000 + c.offset_ms, 1_000_000 + 2_000);
    let audit = stamp_audit(&stamp(1_000_000, 5 * DAY), &c).expect("audit note");
    assert_eq!(audit.code, "sync_freshness.offset_clamped");
    assert_eq!(audit.node, "node-1");
    assert_eq!(audit.claimed_offset_ms, 5 * DAY);
    assert_eq!(audit.applied_offset_ms, 2_000);
}

#[test]
fn boundaries_are_inclusive() {
    let b = bounds();
    let (wall, rec) = (50 * DAY, 50 * DAY);
    assert!(!clamp_offset(2_000, wall, rec, &b).was_clamped);
    assert!(clamp_offset(2_001, wall, rec, &b).was_clamped);
    assert!(!clamp_offset(-7 * DAY, wall, rec, &b).was_clamped);
    let c = clamp_offset(-7 * DAY - 1, wall, rec, &b);
    assert!(c.was_clamped);
    assert_eq!(c.offset_ms, -7 * DAY);
}

#[test]
fn extreme_claims_do_not_overflow() {
    let b = bounds();
    let c = clamp_offset(i64::MIN, 1_000_000, 1_000_000, &b);
    assert!(c.was_clamped);
    assert_eq!(1_000_000 + c.offset_ms, 1_000_000 - 7 * DAY);
    let c = clamp_offset(i64::MAX, 1_000_000, 1_000_000, &b);
    assert!(c.was_clamped);
    assert_eq!(1_000_000 + c.offset_ms, 1_002_000);
}

#[test]
fn extreme_times_do_not_overflow() {
    let b = bounds();
    for wall in [i64::MIN, i64::MAX] {
        for rec in [i64::MIN, 0, i64::MAX] {
            for claim in [i64::MIN, 0, i64::MAX] {
                let _ = clamp_offset(claim, wall, rec, &b);
            }
        }
    }
    let c = clamp_offset(0, i64::MAX, 0, &b);
    assert!(c.was_clamped);
    assert_eq!(c.offset_ms, -i64::MAX + 2_000);
    let c = clamp_offset(0, i64::MIN, 0, &b);
    assert_eq!(c.offset_ms, i64::MAX - 7 * DAY + 1);
    // Garbage so extreme the required offset cannot fit i64 saturates rather than overflowing.
    let c = clamp_offset(0, i64::MIN, i64::MAX, &b);
    assert_eq!(c.offset_ms, i64::MAX);
}

#[test]
fn bounds_are_validated() {
    assert_eq!(Bounds::new(-1, 0).unwrap_err(), FreshnessError::BadBounds);
    assert_eq!(Bounds::new(0, -1).unwrap_err(), FreshnessError::BadBounds);
    assert_eq!(
        Bounds::new(0, 366 * DAY).unwrap_err(),
        FreshnessError::BadBounds
    );
    assert!(Bounds::new(0, 366 * DAY - 1).is_ok());
    assert_eq!(
        FreshnessError::BadBounds.code(),
        "sync_freshness.bad_bounds"
    );
    assert_eq!(Bounds::with_max_age(DAY).expect("ok").tolerance_ms(), 2_000);
}

#[test]
fn trusted_function_marks_findings_unclamped() {
    let ev = mandatory(100);
    let f = find_violations(
        "D",
        &stamp(1_000, 0),
        HubPostedAt::hub_received(1_000),
        &[ev],
    );
    assert_eq!(f.len(), 1);
    assert!(!f[0].clamped);
    assert_eq!(f[0].effective_stamped_at_millis, 1_000);
}

proptest! {
    #[test]
    fn effective_stamp_is_always_inside_the_window(
        claim in any::<i64>(),
        wall in -1_000_000_000_000_000i64..1_000_000_000_000_000,
        rec in -1_000_000_000_000_000i64..1_000_000_000_000_000,
        tol in 0i64..1_000_000,
        age in 0i64..(366 * DAY),
    ) {
        let b = Bounds::new(tol, age).expect("valid");
        let c = clamp_offset(claim, wall, rec, &b);
        let eff = wall + c.offset_ms;
        prop_assert!(eff >= rec - age && eff <= rec + tol);
        prop_assert_eq!(c.was_clamped, c.offset_ms != claim);
    }
}

// WP-SYN-06.T3: the posting time can only come from a clamped offset.

#[test]
fn clamped_posting_time_finds_the_violation_the_claimed_arithmetic_would_miss() {
    let b = Bounds::new(2_000, 7 * DAY).expect("valid");
    let received = 100 * DAY;
    let wall = received - 1_000;
    let ev = mandatory(received - 10 * DAY);
    let st = stamp(wall, -30 * DAY);
    // Control: the old unclamped arithmetic moves the time before the event.
    assert!(st.claimed_hub_time() < ev.published_at);
    let c = clamp_offset(st.hub_offset_ms, wall, received, &b);
    let posted = HubPostedAt::from_clamped(&st, &c).expect("same stamp");
    assert_eq!(posted.millis(), received - 7 * DAY);
    assert!(posted.millis() >= ev.published_at);
    let f = find_violations_clamped("D", &st, received, &b, std::slice::from_ref(&ev))
        .expect("representable");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].effective_stamped_at_millis, posted.millis());
    // Control through the trusted path with the claimed time: nothing found.
    let mut claimed = st.clone();
    claimed.hub_offset_ms = st.hub_offset_ms;
    assert!(
        find_violations(
            "D",
            &claimed,
            HubPostedAt::hub_received(received),
            std::slice::from_ref(&ev)
        )
        .is_empty()
    );
}

#[test]
fn clamp_for_another_stamp_is_refused() {
    let b = bounds();
    let st = stamp(1_000_000, -3 * DAY);
    let other = clamp_offset(5 * DAY, 1_000_000, 1_000_000, &b);
    let e = HubPostedAt::from_clamped(&st, &other).unwrap_err();
    assert_eq!(e, FreshnessError::OffsetNotClamped);
    assert_eq!(e.code(), "sync_freshness.offset_not_clamped");
}

#[test]
fn hub_received_equals_its_argument() {
    assert_eq!(HubPostedAt::hub_received(123).millis(), 123);
}

#[test]
fn unclamped_stamp_gives_the_claimed_hub_time() {
    let b = bounds();
    let st = stamp(100_000, 500);
    let c = clamp_offset(500, 100_000, 100_500, &b);
    assert!(!c.was_clamped);
    let posted = HubPostedAt::from_clamped(&st, &c).expect("same stamp");
    assert_eq!(posted.millis(), st.claimed_hub_time());
}
