//! Per-tenant tracking, scoped re-send confirmation, out-of-scope memory and the tenant bound.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_sync_node::{Position, RecordKey};
use spot_sync_repair_pull::{
    ConditionClear, ConditionPort, ConditionRaise, Input, Output, PullRequest, RepairConfig,
    RepairLoop, ScopeCheck, ScopedResend, forward_conditions,
};

struct Scope;

impl ScopeCheck for Scope {
    fn in_scope(&self, _tenant: &str, key: &RecordKey) -> bool {
        key.entity != "Secret"
    }
    fn scope(&self, _tenant: &str) -> String {
        "branch:khi".to_owned()
    }
    fn applied_prefix(&self, _tenant: &str) -> Option<Position> {
        Some(41)
    }
}

fn k(i: u32) -> RecordKey {
    RecordKey::new("Item", format!("I-{i:07}"))
}

fn secret(i: u32) -> RecordKey {
    RecordKey::new("Secret", format!("S-{i}"))
}

fn missed(tenant: &str, keys: Vec<RecordKey>, now_ms: u64) -> Input {
    Input::Missed {
        tenant: tenant.to_owned(),
        keys,
        bundle_position: 5,
        now_ms,
    }
}

fn arrived(tenant: &str, keys: Vec<RecordKey>) -> Input {
    Input::Arrived {
        tenant: tenant.to_owned(),
        keys,
    }
}

fn resend_complete(tenant: &str, scope: &str) -> Input {
    Input::ResendComplete {
        tenant: tenant.to_owned(),
        scope: scope.to_owned(),
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

fn escalations(out: &[Output]) -> Vec<&ScopedResend> {
    out.iter()
        .filter_map(|o| {
            if let Output::Escalate(r) = o {
                Some(r)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn the_same_key_missing_for_two_tenants_is_two_tracked_keys_asked_separately() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    l.step(missed("t1", vec![k(1)], 0), &s);
    l.step(missed("t2", vec![k(1)], 0), &s);
    assert_eq!(l.tracked_len(), 2);
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(
        out,
        vec![
            Output::Ask(PullRequest {
                tenant: "t1".to_owned(),
                keys: vec![k(1)],
            }),
            Output::Ask(PullRequest {
                tenant: "t2".to_owned(),
                keys: vec![k(1)],
            }),
        ]
    );
}

#[test]
fn a_key_that_arrives_for_one_tenant_does_not_clear_the_other() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    l.step(missed("t1", vec![k(1)], 0), &s);
    l.step(missed("t2", vec![k(1)], 0), &s);
    l.step(Input::Tick(0), &s);
    l.step(arrived("t1", vec![k(1)]), &s);
    assert_eq!(l.tracked_len_for("t1"), 0);
    assert_eq!(l.tracked_len_for("t2"), 1);
    assert_eq!(l.tracked_len(), 1);
    // Much later the other tenant keeps being asked, and only that tenant.
    let out = l.step(Input::Tick(100_000), &s);
    let a = asks(&out);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].tenant, "t2");
    assert_eq!(a[0].keys, vec![k(1)]);
    // An arrival for a tenant that never missed the key changes nothing.
    assert_eq!(l.step(arrived("t3", vec![k(1)]), &s), vec![Output::Nothing]);
    assert_eq!(l.tracked_len_for("t2"), 1);
}

#[test]
fn a_pull_request_never_mixes_tenants() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    l.step(missed("a", (0..3).map(k).collect(), 0), &s);
    l.step(missed("b", (2..5).map(k).collect(), 0), &s);
    let out = l.step(Input::Tick(0), &s);
    let a = asks(&out);
    assert_eq!(a.len(), 2);
    for p in a {
        let expected: Vec<RecordKey> = if p.tenant == "a" {
            (0..3).map(k).collect()
        } else {
            (2..5).map(k).collect()
        };
        assert_eq!(p.keys, expected, "tenant {}", p.tenant);
    }
}

#[derive(Default)]
struct Port {
    raised: Vec<ConditionRaise>,
    cleared: Vec<ConditionClear>,
}

impl ConditionPort for Port {
    fn raise(&mut self, r: &ConditionRaise) {
        self.raised.push(r.clone());
    }
    fn clear(&mut self, c: &ConditionClear) {
        self.cleared.push(c.clone());
    }
}

#[test]
fn the_condition_is_raised_and_cleared_per_node_and_tenant() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    let mut port = Port::default();
    l.step(missed("t1", vec![k(1), k(2)], 0), &s);
    l.step(missed("t2", vec![k(1)], 0), &s);
    let out = l.step(Input::Tick(600_000), &s);
    forward_conditions(&out, &mut port);
    assert_eq!(
        port.raised,
        vec![
            ConditionRaise {
                code: "CLOSURE_UNRESOLVED".to_owned(),
                subject: "node:n1".to_owned(),
                tenant: "t1".to_owned(),
                missing: 2,
            },
            ConditionRaise {
                code: "CLOSURE_UNRESOLVED".to_owned(),
                subject: "node:n1".to_owned(),
                tenant: "t2".to_owned(),
                missing: 1,
            },
        ]
    );
    // The last key of t1 arrives: only t1's condition clears.
    let out = l.step(arrived("t1", vec![k(1), k(2)]), &s);
    forward_conditions(&out, &mut port);
    assert_eq!(
        port.cleared,
        vec![ConditionClear {
            subject: "node:n1".to_owned(),
            tenant: "t1".to_owned(),
        }]
    );
    assert!(!l.condition_raised("t1"));
    assert!(l.condition_raised("t2"));
    // Raised once per tenant: a later tick raises nothing new.
    let out = l.step(Input::Tick(700_000), &s);
    forward_conditions(&out, &mut port);
    assert_eq!(port.raised.len(), 2);
}

#[test]
fn a_flooding_tenant_loses_its_own_oldest_keys_first() {
    let cfg = RepairConfig {
        max_tracked: 10,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope;
    l.step(missed("hostile", (0..10).map(k).collect(), 0), &s);
    l.step(missed("good", vec![k(100), k(101)], 0), &s);
    assert_eq!(l.tracked_len(), 10);
    assert_eq!(l.tracked_len_for("good"), 2);
    assert_eq!(l.tracked_len_for("hostile"), 8);
    assert_eq!(l.counters().dropped_by_bound, 2);
    // The hostile tenant escalates for what was dropped; the good tenant is asked normally.
    let out = l.step(Input::Tick(0), &s);
    let e = escalations(&out);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].tenant, "hostile");
    let a = asks(&out);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].tenant, "good");
    assert_eq!(a[0].keys, vec![k(100), k(101)]);
    // The oldest keys of the hostile tenant went: k(0) and k(1) are not tracked any more.
    l.step(arrived("hostile", (2..10).map(k).collect()), &s);
    assert_eq!(l.tracked_len_for("hostile"), 0);
}

#[test]
fn resend_complete_asks_again_and_forgets_the_dropped_key() {
    let cfg = RepairConfig {
        max_tracked: 2,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope;
    l.step(missed("t1", vec![k(1), k(2), k(3)], 0), &s);
    assert_eq!(l.counters().dropped_by_bound, 1);
    // The drop escalates; single asks stay suppressed while the re-send is outstanding.
    let out = l.step(Input::Tick(0), &s);
    assert_eq!(escalations(&out).len(), 1);
    assert!(asks(&out).is_empty());
    assert!(l.resend_pending("t1"));
    assert_eq!(l.step(Input::Tick(1_000), &s), vec![Output::Nothing]);
    // The hub confirms the re-send: the two keys still missing are asked for again.
    assert_eq!(
        l.step(resend_complete("t1", "branch:khi"), &s),
        vec![Output::Nothing]
    );
    assert!(!l.resend_pending("t1"));
    assert_eq!(l.counters().resends_completed, 1);
    let out = l.step(Input::Tick(1_000), &s);
    let a = asks(&out);
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].keys, vec![k(2), k(3)]);
    assert!(!a[0].keys.contains(&k(1)), "the dropped key is forgotten");
    assert!(escalations(&out).is_empty());
    // They arrive: nothing is left of the tenant, and the dropped key never comes back.
    l.step(arrived("t1", vec![k(2), k(3)]), &s);
    assert_eq!(l.tenant_count(), 0);
    assert_eq!(l.step(Input::Tick(5_000_000), &s), vec![Output::Nothing]);
}

#[test]
fn resend_complete_resets_the_escalation_state() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    l.step(missed("t1", vec![k(1)], 0), &s);
    let mut now = 0;
    let mut first_round_asks = 0;
    loop {
        let out = l.step(Input::Tick(now), &s);
        first_round_asks += asks(&out).len();
        if !escalations(&out).is_empty() {
            break;
        }
        now += 1_000;
        assert!(now < 10_000_000, "no escalation");
    }
    assert_eq!(first_round_asks, 5);
    l.step(resend_complete("t1", "branch:khi"), &s);
    // A second round of five single asks before the next escalation.
    let mut second_round_asks = 0;
    loop {
        now += 1_000;
        let out = l.step(Input::Tick(now), &s);
        second_round_asks += asks(&out).len();
        if !escalations(&out).is_empty() {
            break;
        }
        assert!(now < 100_000_000, "no second escalation");
    }
    assert_eq!(second_round_asks, 5);
}

#[test]
fn resend_complete_for_another_scope_or_without_a_resend_is_ignored() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    l.step(missed("t1", (0..501).map(k).collect(), 0), &s);
    l.step(Input::Tick(0), &s);
    assert!(l.resend_pending("t1"));
    l.step(resend_complete("t1", "branch:other"), &s);
    assert!(l.resend_pending("t1"), "another scope does not confirm it");
    l.step(resend_complete("unknown", "branch:khi"), &s);
    assert_eq!(l.counters().resends_ignored, 2);
    assert_eq!(l.counters().resends_completed, 0);
    // Another tenant's re-send does not reset this tenant.
    l.step(missed("t2", vec![k(1)], 0), &s);
    l.step(resend_complete("t2", "branch:khi"), &s);
    assert!(l.resend_pending("t1"));
}

#[test]
fn a_key_dropped_after_the_re_send_went_out_escalates_again() {
    let cfg = RepairConfig {
        max_tracked: 2,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope;
    l.step(missed("t1", vec![k(1), k(2), k(3)], 0), &s);
    l.step(Input::Tick(0), &s);
    // A new miss drops k(2) after the first re-send was emitted: it is not covered by it.
    l.step(missed("t1", vec![k(4)], 10), &s);
    l.step(resend_complete("t1", "branch:khi"), &s);
    let out = l.step(Input::Tick(1_000), &s);
    assert_eq!(escalations(&out).len(), 1);
}

#[test]
fn an_out_of_scope_key_is_reported_once_until_it_is_seen_applied() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    let out = l.step(missed("t1", vec![secret(1), k(1)], 0), &s);
    assert_eq!(
        out,
        vec![Output::OutOfScope {
            tenant: "t1".to_owned(),
            keys: vec![secret(1)],
        }]
    );
    assert_eq!(
        l.step(missed("t1", vec![secret(1)], 10), &s),
        vec![Output::Nothing]
    );
    assert_eq!(l.counters().out_of_scope, 1);
    assert_eq!(l.counters().out_of_scope_repeats, 1);
    assert_eq!(l.out_of_scope_len("t1"), 1);
    // Seen applied: forgotten, so a later miss is reported again.
    l.step(arrived("t1", vec![secret(1)]), &s);
    assert_eq!(l.out_of_scope_len("t1"), 0);
    let out = l.step(missed("t1", vec![secret(1)], 20), &s);
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0], Output::OutOfScope { keys, .. } if keys.as_slice() == [secret(1)]));
    // The memory is per tenant: another tenant is told about the same key.
    let out = l.step(missed("t2", vec![secret(1)], 30), &s);
    assert!(matches!(&out[0], Output::OutOfScope { tenant, .. } if tenant == "t2"));
}

#[test]
fn the_out_of_scope_memory_is_bounded_and_the_oldest_is_forgotten() {
    let cfg = RepairConfig {
        max_out_of_scope: 2,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope;
    let out = l.step(missed("t1", vec![secret(1), secret(2), secret(3)], 0), &s);
    assert_eq!(
        out,
        vec![Output::OutOfScope {
            tenant: "t1".to_owned(),
            keys: vec![secret(1), secret(2), secret(3)],
        }]
    );
    assert_eq!(l.out_of_scope_len("t1"), 2);
    assert_eq!(l.counters().out_of_scope_forgotten, 1);
    // Still remembered: not reported again.
    assert_eq!(
        l.step(missed("t1", vec![secret(3)], 10), &s),
        vec![Output::Nothing]
    );
    // Forgotten beyond the bound: reported again, and the set stays at the bound.
    let out = l.step(missed("t1", vec![secret(1)], 20), &s);
    assert!(matches!(&out[0], Output::OutOfScope { keys, .. } if keys.as_slice() == [secret(1)]));
    assert_eq!(l.out_of_scope_len("t1"), 2);
    assert_eq!(l.counters().out_of_scope_forgotten, 2);
    assert_eq!(l.counters().out_of_scope, 4);
}

#[test]
fn tenants_beyond_the_bound_are_refused_not_tracked() {
    let cfg = RepairConfig {
        max_tenants: 2,
        ..RepairConfig::default()
    };
    let mut l = new_loop(cfg);
    let s = Scope;
    l.step(missed("t1", vec![k(1)], 0), &s);
    l.step(missed("t2", vec![k(1)], 0), &s);
    let out = l.step(missed("t3", vec![k(1)], 0), &s);
    assert_eq!(out, vec![Output::TenantRefused("t3".to_owned())]);
    assert_eq!(l.tenant_count(), 2);
    assert_eq!(l.tracked_len_for("t3"), 0);
    assert_eq!(l.counters().tenants_refused, 1);
    // A known tenant still works, and a freed slot is reusable.
    l.step(missed("t1", vec![k(2)], 0), &s);
    assert_eq!(l.tracked_len_for("t1"), 2);
    l.step(arrived("t2", vec![k(1)]), &s);
    assert_eq!(l.tenant_count(), 1);
    l.step(missed("t3", vec![k(1)], 0), &s);
    assert_eq!(l.tracked_len_for("t3"), 1);
}

#[test]
fn a_tenant_with_nothing_left_leaves_no_state() {
    let mut l = new_loop(RepairConfig::default());
    let s = Scope;
    l.step(missed("t1", vec![k(1)], 0), &s);
    assert_eq!(l.tenant_count(), 1);
    l.step(arrived("t1", vec![k(1)]), &s);
    assert_eq!(l.tenant_count(), 0);
}

proptest! {
    #[test]
    fn requests_only_name_keys_missing_for_their_own_tenant(
        steps in proptest::collection::vec((0u8..3, 0u8..3, 0u32..20), 1..120),
        cap in 1usize..30,
    ) {
        let cfg = RepairConfig { max_tracked: cap, ..RepairConfig::default() };
        let mut l = RepairLoop::new("n1", cfg).expect("config");
        let s = Scope;
        let mut model: BTreeSet<(String, RecordKey)> = BTreeSet::new();
        let mut now = 0u64;
        for (kind, tenant, key) in steps {
            let tenant = format!("t{tenant}");
            let key = k(key);
            let out = match kind {
                0 => {
                    model.insert((tenant.clone(), key.clone()));
                    l.step(missed(&tenant, vec![key], now), &s)
                }
                1 => {
                    model.remove(&(tenant.clone(), key.clone()));
                    l.step(arrived(&tenant, vec![key]), &s)
                }
                _ => {
                    now += 40_000;
                    l.step(Input::Tick(now), &s)
                }
            };
            for p in asks(&out) {
                for asked in &p.keys {
                    prop_assert!(model.contains(&(p.tenant.clone(), asked.clone())));
                }
            }
            prop_assert!(l.tracked_len() <= cap);
            let per_tenant: usize = ["t0", "t1", "t2"].iter().map(|t| l.tracked_len_for(t)).sum();
            prop_assert_eq!(per_tenant, l.tracked_len());
        }
    }
}
