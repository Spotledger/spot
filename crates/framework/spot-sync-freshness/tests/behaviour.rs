//! Behaviour of the freshness tracker, the posting decision, the stamp, the offset estimate and
//! the stale-document check. Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_sync_freshness::{
    ClockSample, FindingKind, FreshnessError, FreshnessSettings, FreshnessTracker, HubPostedAt,
    MandatoryEvent, PostDecision, StalePolicy, StampInput, StampOverride, SyncStamp,
    WarningConfirmed, build_stamp, decide, find_violations, hub_offset_estimate, list_retroactive,
};

const H: u64 = 60 * 60 * 1000;

fn s(x: &str) -> String {
    x.to_owned()
}

#[test]
fn stale_exactly_at_threshold_is_not_stale_one_past_is() {
    let settings = FreshnessSettings::default();
    let mut t = FreshnessTracker::new();
    t.confirm("tax", 1_000);
    assert!(!t.is_stale("tax", 1_000 + 48 * H, &settings));
    assert!(t.is_stale("tax", 1_000 + 48 * H + 1, &settings));
    assert_eq!(t.elapsed("tax", 1_000 + 5), Some(5));
}

#[test]
fn never_confirmed_is_stale_and_per_class_setting_applies() {
    let mut settings = FreshnessSettings::default();
    settings.per_class.insert(s("tax"), 2 * H);
    let mut t = FreshnessTracker::new();
    assert!(t.is_stale("tax", 0, &settings));
    assert_eq!(t.elapsed("tax", 10), None);
    t.confirm("tax", 0);
    t.confirm("price", 0);
    assert!(t.is_stale("tax", 2 * H + 1, &settings));
    assert!(!t.is_stale("price", 2 * H + 1, &settings));
}

#[test]
fn wall_clock_movement_changes_nothing() {
    // The tracker takes no wall-clock input: two stamps built at wildly different wall clocks
    // with the same monotonic instants report the same staleness.
    let settings = FreshnessSettings::default();
    let mut t = FreshnessTracker::new();
    t.confirm_with_horizon("tax", 7, 100);
    let classes = [s("tax")];
    let mk = |wall: i64| {
        build_stamp(
            &StampInput {
                node: "n1",
                stamped_at_millis: wall,
                hub_offset_ms: 0,
                applied_prefix: 7,
                classes: &classes,
                now_monotonic_millis: 100 + 49 * H,
                metadata_version: 3,
                software_version: "1.0.0",
            },
            &t,
            &settings,
            None,
            None,
        )
    };
    let a = mk(1_700_000_000_000);
    let b = mk(-5_000_000_000_000);
    assert_eq!(a.stale, b.stale);
    assert_eq!(a.stale, vec![s("tax")]);
}

#[test]
fn confirm_if_covered_needs_the_prefix_to_reach_the_horizon() {
    let mut t = FreshnessTracker::new();
    assert!(!t.confirm_if_covered("tax", 9, 10, 5));
    assert_eq!(t.elapsed("tax", 6), None);
    assert!(t.confirm_if_covered("tax", 10, 10, 5));
    assert_eq!(t.confirmed_horizons().get("tax"), Some(&10));
}

#[test]
fn decide_table() {
    let stale = [s("tax")];
    let req = [s("tax")];
    // nothing relevant is stale
    assert_eq!(
        decide(&[], &req, StalePolicy::Block, false),
        PostDecision::Allow
    );
    assert_eq!(
        decide(&stale, &[s("price")], StalePolicy::Block, false),
        PostDecision::Allow
    );
    // allow_stamp
    assert_eq!(
        decide(&stale, &req, StalePolicy::AllowStamp, false),
        PostDecision::AllowStamped
    );
    // warn
    match decide(&stale, &req, StalePolicy::Warn, true) {
        PostDecision::NeedWarningConfirmed { text_key, params } => {
            assert_eq!(text_key, "sync_stale_warn");
            assert_eq!(params.get("classes").map(String::as_str), Some("tax"));
        }
        other => panic!("{other:?}"),
    }
    // block without and with the permission
    assert!(matches!(
        decide(&stale, &req, StalePolicy::Block, false),
        PostDecision::Block { .. }
    ));
    let o = decide(&stale, &req, StalePolicy::Block, true);
    assert_eq!(
        o,
        PostDecision::AllowWithOverride {
            stale: vec![s("tax")]
        }
    );
    assert!(o.requires_audit());
    assert!(!PostDecision::AllowStamped.requires_audit());
}

#[test]
fn the_platform_default_policy_is_allow_stamp() {
    assert_eq!(StalePolicy::default(), StalePolicy::AllowStamp);
}

fn golden_stamp() -> SyncStamp {
    let mut t = FreshnessTracker::new();
    t.confirm_with_horizon("tax", 1200, 0);
    build_stamp(
        &StampInput {
            node: "branch-lhr-01",
            stamped_at_millis: 1_793_000_000_000,
            hub_offset_ms: -1_250,
            applied_prefix: 1234,
            classes: &[s("tax"), s("price")],
            now_monotonic_millis: 50 * H,
            metadata_version: 42,
            software_version: "0.9.3",
        },
        &t,
        &FreshnessSettings::default(),
        None,
        None,
    )
}

#[test]
fn golden_stamp_json() {
    let json = serde_json::to_string(&golden_stamp()).unwrap();
    assert_eq!(
        json,
        r#"{"node":"branch-lhr-01","stamped_at_millis":1793000000000,"hub_offset_ms":-1250,"applied_prefix":1234,"class_horizons_confirmed":{"tax":1200},"stale":["tax","price"],"metadata_version":42,"software_version":"0.9.3"}"#
    );
    assert!(json.len() < 400, "stamp is {} bytes", json.len());
    let back: SyncStamp = serde_json::from_str(&json).unwrap();
    assert_eq!(back, golden_stamp());
}

#[test]
fn golden_stamp_with_override_and_warning() {
    let mut st = golden_stamp();
    st.override_ = Some(StampOverride {
        actor: s("sup1"),
        classes: vec![s("tax")],
    });
    st.warning_confirmed = Some(WarningConfirmed { actor: s("u1") });
    let json = serde_json::to_string(&st).unwrap();
    assert!(json.contains(r#""override":{"actor":"sup1","classes":["tax"]}"#));
    assert!(json.contains(r#""warning_confirmed":{"actor":"u1"}"#));
    assert!(json.len() < 400);
    assert_eq!(serde_json::from_str::<SyncStamp>(&json).unwrap(), st);
}

#[test]
fn offset_estimate_symmetric_path() {
    // hub is 5000 ms ahead, 100 ms each way, 10 ms hub processing.
    let sample = ClockSample {
        node_sent: 1_000,
        hub_received: 1_000 + 100 + 5_000,
        hub_sent: 1_000 + 110 + 5_000,
        node_received: 1_000 + 210,
    };
    let e = hub_offset_estimate(&[sample]).unwrap();
    assert_eq!(e.offset_ms, 5_000);
    assert_eq!(e.uncertainty_ms, 100);
}

#[test]
fn offset_estimate_keeps_best_round_trip() {
    let slow = ClockSample {
        node_sent: 0,
        hub_received: 900 + 100,
        hub_sent: 900 + 100,
        node_received: 1_800, // asymmetric, delay 1800
    };
    let fast = ClockSample {
        node_sent: 10_000,
        hub_received: 10_000 + 20 + 100,
        hub_sent: 10_000 + 20 + 100,
        node_received: 10_040,
    };
    let e = hub_offset_estimate(&[slow, fast]).unwrap();
    assert_eq!(e.offset_ms, 100);
    assert_eq!(e.uncertainty_ms, 20);
}

#[test]
fn offset_estimate_errors() {
    assert_eq!(hub_offset_estimate(&[]), Err(FreshnessError::NoSamples));
    let bad = ClockSample {
        node_sent: 100,
        hub_received: 0,
        hub_sent: 0,
        node_received: 50,
    };
    assert_eq!(
        hub_offset_estimate(&[bad]),
        Err(FreshnessError::BadSample { index: 0 })
    );
    assert_eq!(
        FreshnessError::NoSamples.code(),
        "sync_freshness.no_samples"
    );
    assert_eq!(
        FreshnessError::BadSample { index: 0 }.code(),
        "sync_freshness.bad_sample"
    );
}

proptest! {
    #[test]
    fn offset_recovers_true_offset_within_uncertainty(
        true_offset in -10_000_000i64..10_000_000,
        up in 0i64..5_000,
        down in 0i64..5_000,
        proc_ms in 0i64..100,
        t0 in 0i64..1_000_000_000,
    ) {
        let hub_received = t0 + up + true_offset;
        let hub_sent = hub_received + proc_ms;
        let node_received = hub_sent - true_offset + down;
        let e = hub_offset_estimate(&[ClockSample {
            node_sent: t0, hub_received, hub_sent, node_received,
        }]).unwrap();
        let err = e.offset_ms.abs_diff(true_offset);
        prop_assert!(err <= e.uncertainty_ms);
    }

    #[test]
    fn staleness_threshold_is_exact(max in 0u64..1_000_000, at in 0u64..1_000_000) {
        let mut settings = FreshnessSettings::default();
        settings.per_class.insert(s("k"), max);
        let mut t = FreshnessTracker::new();
        t.confirm("k", at);
        prop_assert!(!t.is_stale("k", at + max, &settings));
        prop_assert!(t.is_stale("k", at + max + 1, &settings));
    }
}

fn ev(
    position: u64,
    published_at: i64,
    effective_from: Option<i64>,
    record: &str,
) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at,
        effective_from,
        record: s(record),
    }
}

fn stamp_at(wall: i64, offset: i64, prefix: u64) -> SyncStamp {
    let mut st = golden_stamp();
    st.stamped_at_millis = wall;
    st.hub_offset_ms = offset;
    st.applied_prefix = prefix;
    st
}

#[test]
fn finding_for_a_rule_in_force_and_not_applied() {
    let st = stamp_at(10_000, 0, 5);
    let events = [ev(6, 4_000, None, "TaxRate/gst@2")];
    let f = find_violations("INV-1", &st, HubPostedAt::hub_received(10_000), &events);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].kind, FindingKind::StampViolation);
    assert_eq!(f[0].document, "INV-1");
    assert_eq!(f[0].mandatory_record, "TaxRate/gst@2");
    assert_eq!(f[0].node, "branch-lhr-01");
    assert_eq!(f[0].elapsed_millis, 6_000);
}

#[test]
fn applied_event_is_not_a_violation() {
    let st = stamp_at(10_000, 0, 6);
    assert!(
        find_violations(
            "D",
            &st,
            HubPostedAt::hub_received(10_000),
            &[ev(6, 4_000, None, "r")]
        )
        .is_empty()
    );
}

#[test]
fn published_exactly_at_stamped_at_counts() {
    let st = stamp_at(10_000, 0, 0);
    assert_eq!(
        find_violations(
            "D",
            &st,
            HubPostedAt::hub_received(10_000),
            &[ev(1, 10_000, None, "r")]
        )
        .len(),
        1
    );
    assert!(
        find_violations(
            "D",
            &st,
            HubPostedAt::hub_received(10_000),
            &[ev(1, 10_001, None, "r")]
        )
        .is_empty()
    );
}

#[test]
fn skew_corrected_comparison_with_a_node_clock_ten_minutes_fast() {
    // True time 100_000_000; the node reads 10 minutes ahead, so offset is -600_000.
    let ten_min = 600_000;
    let true_now = 100_000_000;
    let st = stamp_at(true_now + ten_min, -ten_min, 0);
    // published 5 minutes before the true posting time: the node was told late, so a violation.
    let early = ev(1, true_now - 300_000, None, "early");
    // published 5 minutes after the true posting time: raw stamped_at would wrongly include it.
    let late = ev(2, true_now + 300_000, None, "late");
    let f = find_violations(
        "D",
        &st,
        HubPostedAt::hub_received(true_now),
        &[early.clone(), late.clone()],
    );
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].mandatory_record, "early");
    let both = [early, late];
    let r = list_retroactive(&st, HubPostedAt::hub_received(true_now), &both);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].record, "late");
}

#[test]
fn retroactive_correction_is_listed_not_a_violation() {
    let st = stamp_at(10_000, 0, 0);
    // published after posting, effective from before the posting
    let events = [ev(3, 20_000, Some(5_000), "fix")];
    assert!(find_violations("D", &st, HubPostedAt::hub_received(10_000), &events).is_empty());
    let r = list_retroactive(&st, HubPostedAt::hub_received(10_000), &events);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].record, "fix");
}

#[test]
fn effective_date_not_yet_reached_is_not_a_violation() {
    let st = stamp_at(10_000, 0, 0);
    let events = [ev(3, 1_000, Some(50_000), "future")];
    assert!(find_violations("D", &st, HubPostedAt::hub_received(10_000), &events).is_empty());
    // and a future-effective event published later is not a retroactive correction either
    let events = [ev(3, 20_000, Some(50_000), "future")];
    assert!(list_retroactive(&st, HubPostedAt::hub_received(10_000), &events).is_empty());
}

#[test]
fn findings_come_back_in_position_order() {
    let st = stamp_at(10_000, 0, 0);
    let events = [ev(9, 1_000, None, "b"), ev(2, 1_000, None, "a")];
    let f = find_violations("D", &st, HubPostedAt::hub_received(10_000), &events);
    assert_eq!(f[0].mandatory_record, "a");
    assert_eq!(f[1].mandatory_record, "b");
}
