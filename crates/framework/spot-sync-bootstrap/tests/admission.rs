#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Admission control: many nodes returning together.

use spot_sync_bootstrap::{Admission, AdmissionSettings, Decision};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn a_hundred_nodes_never_exceed_the_concurrency_limit_and_all_finish() {
    let settings = AdmissionSettings::default();
    let limit = settings.max_concurrent_bootstraps;
    let mut adm = Admission::new(settings).unwrap();
    let run_ms = 20_000_i64;

    // (time, kind, node): kind 0 = a bootstrap finishes (releases first), 1 = a node asks.
    let mut events: BTreeMap<(i64, u8, usize), ()> = BTreeMap::new();
    for n in 0..100 {
        events.insert((0, 1, n), ());
    }
    let mut first_delays = BTreeSet::new();
    let mut peak = 0_usize;
    let mut done = BTreeSet::new();
    let mut now = 0_i64;
    while let Some((&(t, kind, n), ())) = events.iter().next() {
        events.remove(&(t, kind, n));
        now = now.max(t);
        let id = format!("node-{n}");
        if kind == 0 {
            adm.release(&id);
            done.insert(n);
            continue;
        }
        match adm.admit(&id, t) {
            Decision::Admit => {
                peak = peak.max(adm.running());
                events.insert((t + run_ms, 0, n), ());
            }
            Decision::RetryAfter(ms) => {
                if t == 0 {
                    first_delays.insert(ms);
                }
                assert!((5_000..60_000).contains(&ms), "{ms}");
                events.insert((t + i64::try_from(ms).unwrap(), 1, n), ());
            }
        }
        assert!(adm.running() <= limit);
    }
    assert_eq!(peak, limit);
    assert_eq!(done.len(), 100);
    assert_eq!(adm.running(), 0);
    // 92 nodes were refused at time zero and their delays spread out, not one shared value.
    assert!(first_delays.len() >= 50, "{}", first_delays.len());
    assert!(now > 0);
}

#[test]
fn the_retry_delay_is_stable_per_node() {
    let adm = Admission::new(AdmissionSettings::default()).unwrap();
    assert_eq!(adm.retry_delay_ms("a"), adm.retry_delay_ms("a"));
    assert_ne!(adm.retry_delay_ms("a"), adm.retry_delay_ms("b"));
}

#[test]
fn an_admitted_node_asking_again_keeps_one_slot() {
    let settings = AdmissionSettings {
        max_concurrent_bootstraps: 1,
        ..AdmissionSettings::default()
    };
    let mut adm = Admission::new(settings).unwrap();
    assert_eq!(adm.admit("a", 0), Decision::Admit);
    assert_eq!(adm.admit("a", 10), Decision::Admit);
    assert_eq!(adm.running(), 1);
    assert!(matches!(adm.admit("b", 10), Decision::RetryAfter(_)));
    adm.release("a");
    assert_eq!(adm.admit("b", 20), Decision::Admit);
}

#[test]
fn chunks_are_paced_by_a_token_bucket() {
    let mut adm = Admission::new(AdmissionSettings::default()).unwrap();
    assert_eq!(
        adm.chunk("a", 0).unwrap_err().code(),
        "sync_bootstrap.not_admitted"
    );
    adm.admit("a", 0);
    for _ in 0..4 {
        assert_eq!(adm.chunk("a", 0).unwrap(), Decision::Admit);
    }
    assert_eq!(adm.chunk("a", 0).unwrap(), Decision::RetryAfter(250));
    assert_eq!(adm.chunk("a", 100).unwrap(), Decision::RetryAfter(150));
    assert_eq!(adm.chunk("a", 250).unwrap(), Decision::Admit);
    assert_eq!(adm.chunk("a", 250).unwrap(), Decision::RetryAfter(250));
    // A long quiet period refills only to the burst.
    for _ in 0..4 {
        assert_eq!(adm.chunk("a", 1_000_000).unwrap(), Decision::Admit);
    }
    assert!(matches!(
        adm.chunk("a", 1_000_000).unwrap(),
        Decision::RetryAfter(_)
    ));
    // Another node has its own bucket.
    adm.admit("b", 0);
    assert_eq!(adm.chunk("b", 0).unwrap(), Decision::Admit);
}

#[test]
fn unusable_settings_are_refused() {
    for bad in [
        AdmissionSettings {
            max_concurrent_bootstraps: 0,
            ..AdmissionSettings::default()
        },
        AdmissionSettings {
            chunk_interval_ms: 0,
            ..AdmissionSettings::default()
        },
        AdmissionSettings {
            chunk_burst: 0,
            ..AdmissionSettings::default()
        },
    ] {
        assert_eq!(
            Admission::new(bad).err().unwrap().code(),
            "sync_bootstrap.invalid_settings"
        );
    }
}
