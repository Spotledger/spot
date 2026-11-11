//! Behaviour of the closure repair loop.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_sync_closure::{Position, RecordKey};
use spot_sync_repair_pull::{
    CLOSURE_UNRESOLVED, ConditionPort, ConditionRaise, Input, OUT_OF_SCOPE, Output, PullRequest,
    RepairConfig, RepairLoop, ScopeCheck, forward_conditions,
};

struct Scope {
    foreign_entity: &'static str,
    prefix: Option<Position>,
}

impl Scope {
    fn all() -> Self {
        Self {
            foreign_entity: "Secret",
            prefix: Some(41),
        }
    }
}

impl ScopeCheck for Scope {
    fn in_scope(&self, key: &RecordKey) -> bool {
        key.entity != self.foreign_entity
    }
    fn scope(&self) -> String {
        "branch:khi".to_owned()
    }
    fn applied_prefix(&self) -> Option<Position> {
        self.prefix
    }
}

fn k(i: u32) -> RecordKey {
    RecordKey::new("Item", format!("I-{i:07}"))
}

fn keys(r: std::ops::Range<u32>) -> Vec<RecordKey> {
    r.map(k).collect()
}

fn missed(keys: Vec<RecordKey>, pos: Position, now_ms: u64) -> Input {
    Input::Missed {
        keys,
        bundle_position: pos,
        now_ms,
    }
}

fn new_loop(cfg: RepairConfig) -> RepairLoop {
    RepairLoop::new("n1", cfg).expect("config")
}

fn asks(out: &[Output]) -> Vec<&PullRequest> {
    out.iter()
        .filter_map(|o| {
            if let Output::Ask(p) = o {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}

fn escalations(out: &[Output]) -> usize {
    out.iter()
        .filter(|o| matches!(o, Output::Escalate(_)))
        .count()
}

#[test]
fn asks_only_on_tick_and_names_keys_only() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope::all();
    assert_eq!(l.step(missed(keys(0..3), 50, 0), &s), vec![Output::Nothing]);
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(out, vec![Output::Ask(PullRequest { keys: keys(0..3) })]);
}

#[test]
fn a_key_is_not_asked_again_within_the_ask_interval() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope::all();
    l.step(missed(keys(0..2), 50, 0), &s);
    assert_eq!(asks(&l.step(Input::Tick(0), &s)).len(), 1);
    // Re-reported misses and early ticks add nothing.
    l.step(missed(keys(0..2), 50, 10_000), &s);
    assert_eq!(l.step(Input::Tick(29_999), &s), vec![Output::Nothing]);
    assert_eq!(l.counters().duplicate_misses, 2);
    // A new key is asked alone while the old ones are still waiting.
    l.step(missed(vec![k(9)], 60, 30_000), &s);
    let out = l.step(Input::Tick(30_000), &s);
    assert_eq!(asks(&out)[0].keys, vec![k(9)]);
}

#[test]
fn requests_are_batched_at_max_keys_lowest_position_first() {
    let cfg = RepairConfig {
        max_keys: 200,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope::all();
    l.step(missed(keys(300..400), 90, 0), &s);
    l.step(missed(keys(0..300), 10, 0), &s);
    let out = l.step(Input::Tick(0), &s);
    let a = asks(&out);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].keys.len(), 200);
    assert_eq!(a[1].keys.len(), 200);
    assert_eq!(
        a[0].keys[0],
        k(0),
        "the key that holds the prefix back goes first"
    );
    assert_eq!(a[1].keys[199], k(399));
    assert!(escalations(&out) == 0);
}

#[test]
fn backoff_schedule_is_golden() {
    let cfg = RepairConfig {
        jitter_seed: 7,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope::all();
    l.step(missed(vec![k(1)], 5, 0), &s);
    let mut times = Vec::new();
    let mut now = 0;
    for _ in 0..4 {
        // Step to the next time the key is asked.
        loop {
            if !asks(&l.step(Input::Tick(now), &s)).is_empty() {
                times.push(now);
                break;
            }
            now += 1;
        }
    }
    assert_eq!(times, GOLDEN);
}

const GOLDEN: [u64; 4] = [0, 33_137, 95_165, 236_008];

#[test]
fn backoff_doubles_and_is_capped_without_jitter_drift() {
    let cfg = RepairConfig {
        max_asks_per_key: 100,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope::all();
    l.step(missed(vec![k(1)], 5, 0), &s);
    let mut times = vec![];
    let mut now = 0;
    while times.len() < 12 {
        if !asks(&l.step(Input::Tick(now), &s)).is_empty() {
            times.push(now);
        }
        now += 500;
    }
    let gaps: Vec<u64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[0] >= 30_000 && gaps[0] <= 38_000, "{gaps:?}");
    assert!(gaps[1] >= 60_000, "{gaps:?}");
    assert!(gaps.iter().all(|g| *g <= 900_500), "{gaps:?}");
    assert!(*gaps.last().expect("gap") >= 900_000 - 500);
}

#[test]
fn same_seed_same_schedule_other_seed_differs() {
    fn run(seed: u64) -> Vec<u64> {
        let cfg = RepairConfig {
            jitter_seed: seed,
            max_asks_per_key: 100,
            ..RepairConfig::default()
        };
        let mut l = RepairLoop::new("n1", cfg).expect("config");
        let s = Scope::all();
        l.step(missed(keys(0..5), 5, 0), &s);
        let mut times = vec![];
        let mut now = 0;
        while times.len() < 6 {
            if !asks(&l.step(Input::Tick(now), &s)).is_empty() {
                times.push(now);
            }
            now += 100;
        }
        times
    }
    assert_eq!(run(1), run(1));
    assert_ne!(run(1), run(2));
}

#[test]
fn escalates_after_max_asks_per_key_then_suppresses_single_asks() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope::all();
    l.step(missed(vec![k(1)], 5, 0), &s);
    let mut now = 0;
    let mut asked = 0;
    let mut escalated_at = None;
    while now < 3_600_000 && escalated_at.is_none() {
        let out = l.step(Input::Tick(now), &s);
        asked += asks(&out).len();
        if escalations(&out) > 0 {
            escalated_at = Some(now);
            assert_eq!(
                out.iter().find_map(|o| match o {
                    Output::Escalate(r) => Some(r.clone()),
                    _ => None,
                }),
                Some(spot_sync_repair_pull::ScopedResend {
                    scope: "branch:khi".to_owned(),
                    from_position: Some(41),
                })
            );
        }
        now += 1_000;
    }
    assert_eq!(asked, 5);
    assert!(escalated_at.is_some());
    assert!(l.resend_pending());
    // No single ask while the re-send is outstanding.
    let out = l.step(Input::Tick(escalated_at.unwrap_or(0) + 1), &s);
    assert!(asks(&out).is_empty());
    // The keys arrive: the re-send completes.
    l.step(Input::Arrived(vec![k(1)]), &s);
    assert!(!l.resend_pending());
    assert_eq!(l.tracked_len(), 0);
}

#[test]
fn escalates_by_volume_once() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope::all();
    l.step(missed(keys(0..501), 5, 0), &s);
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(escalations(&out), 1);
    assert!(asks(&out).is_empty());
    // Still suppressed on the next ticks, and exactly at 500 it would not have escalated.
    assert_eq!(l.step(Input::Tick(1_000), &s), vec![Output::Nothing]);
    let mut m = new_loop(RepairConfig::default());
    m.step(missed(keys(0..500), 5, 0), &s);
    let out = m.step(Input::Tick(0), &s);
    assert_eq!(escalations(&out), 0);
    assert_eq!(asks(&out).len(), 3);
}

#[derive(Default)]
struct Port {
    raised: Vec<ConditionRaise>,
    cleared: Vec<String>,
}
impl ConditionPort for Port {
    fn raise(&mut self, r: &ConditionRaise) {
        self.raised.push(r.clone());
    }
    fn clear(&mut self, subject: &str) {
        self.cleared.push(subject.to_owned());
    }
}

#[test]
fn condition_raises_once_after_ten_minutes_and_clears_with_the_last_key() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope::all();
    let mut port = Port::default();
    l.step(missed(keys(0..2), 5, 0), &s);
    let out = l.step(Input::Tick(599_999), &s);
    forward_conditions(&out, &mut port);
    assert!(port.raised.is_empty());
    let out = l.step(Input::Tick(600_000), &s);
    forward_conditions(&out, &mut port);
    assert_eq!(
        port.raised,
        vec![ConditionRaise {
            code: CLOSURE_UNRESOLVED.to_owned(),
            subject: "node:n1".to_owned(),
            missing: 2,
        }]
    );
    // Not raised twice.
    let out = l.step(Input::Tick(700_000), &s);
    forward_conditions(&out, &mut port);
    assert_eq!(port.raised.len(), 1);
    // One key arrives: still raised. The last one clears it.
    let out = l.step(Input::Arrived(vec![k(0)]), &s);
    forward_conditions(&out, &mut port);
    assert!(port.cleared.is_empty() && l.condition_raised());
    let out = l.step(Input::Arrived(vec![k(1)]), &s);
    forward_conditions(&out, &mut port);
    assert_eq!(port.cleared, vec!["node:n1".to_owned()]);
    assert!(!l.condition_raised());
}

#[test]
fn out_of_scope_key_goes_to_the_lead_and_is_never_asked() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope::all();
    let secret = RecordKey::new("Secret", "S-1");
    let out = l.step(missed(vec![secret.clone(), k(1)], 5, 0), &s);
    assert_eq!(out, vec![Output::OutOfScope(vec![secret.clone()])]);
    assert_eq!(OUT_OF_SCOPE, "sync_repair_pull.out_of_scope");
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(asks(&out)[0].keys, vec![k(1)]);
    assert_eq!(l.tracked_len(), 1);
    assert_eq!(l.counters().out_of_scope, 1);
}

#[test]
fn a_scope_that_narrows_drops_tracked_keys_to_the_lead() {
    let mut l = new_loop(RepairConfig::default());
    let wide = Scope {
        foreign_entity: "None",
        prefix: None,
    };
    l.step(missed(vec![RecordKey::new("Secret", "S-1")], 5, 0), &wide);
    let narrow = Scope::all();
    let out = l.step(Input::Tick(0), &narrow);
    assert_eq!(
        out,
        vec![Output::OutOfScope(vec![RecordKey::new("Secret", "S-1")])]
    );
    assert_eq!(l.tracked_len(), 0);
}

#[test]
fn memory_is_bounded_under_a_million_distinct_misses() {
    let cfg = RepairConfig {
        max_tracked: 5_000,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope::all();
    for chunk in 0..1_000u32 {
        l.step(missed(keys(chunk * 1_000..(chunk + 1) * 1_000), 5, 0), &s);
        assert!(l.tracked_len() <= 5_000);
    }
    assert_eq!(l.tracked_len(), 5_000);
    assert_eq!(l.counters().dropped_by_bound, 1_000_000 - 5_000);
    // The dropped keys are not lost: the loop escalates for a scoped re-send.
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(escalations(&out), 1);
}

#[test]
fn a_dropped_key_escalates_even_when_nothing_else_is_missing() {
    let cfg = RepairConfig {
        max_tracked: 1,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope::all();
    l.step(missed(vec![k(1), k(2)], 5, 0), &s);
    l.step(Input::Arrived(vec![k(2)]), &s);
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(escalations(&out), 1);
}

#[test]
fn unusable_settings_are_refused() {
    for cfg in [
        RepairConfig {
            max_keys: 0,
            ..RepairConfig::default()
        },
        RepairConfig {
            ask_interval_ms: 0,
            ..RepairConfig::default()
        },
        RepairConfig {
            max_interval_ms: 1,
            ..RepairConfig::default()
        },
        RepairConfig {
            max_asks_per_key: 0,
            ..RepairConfig::default()
        },
        RepairConfig {
            max_tracked: 0,
            ..RepairConfig::default()
        },
    ] {
        let err = RepairLoop::new("n1", cfg).expect_err("refused");
        assert_eq!(err.code(), "sync_repair_pull.invalid_config");
    }
}

#[test]
fn every_code_is_in_the_catalogue() {
    let toml = include_str!("../../spot-errors/catalogue/sync_repair_pull.toml");
    for code in [OUT_OF_SCOPE, spot_sync_repair_pull::INVALID_CONFIG] {
        assert!(toml.contains(&format!("code = \"{code}\"")), "{code}");
    }
}

proptest! {
    #[test]
    fn tracked_set_never_exceeds_the_bound_and_arrivals_empty_it(
        batches in proptest::collection::vec(proptest::collection::vec(0u32..400, 0..40), 1..20),
        cap in 1usize..60,
    ) {
        let cfg = RepairConfig { max_tracked: cap, ..RepairConfig::default() };
        let mut l = RepairLoop::new("n1", cfg).expect("config");
        let s = Scope::all();
        let mut all = BTreeSet::new();
        let mut now = 0;
        for b in &batches {
            let ks: Vec<RecordKey> = b.iter().map(|i| k(*i)).collect();
            all.extend(b.iter().copied());
            l.step(missed(ks, 5, now), &s);
            l.step(Input::Tick(now), &s);
            now += 1_000;
            prop_assert!(l.tracked_len() <= cap);
        }
        l.step(Input::Arrived(all.iter().map(|i| k(*i)).collect()), &s);
        prop_assert_eq!(l.tracked_len(), 0);
        prop_assert!(!l.resend_pending());
    }
}
