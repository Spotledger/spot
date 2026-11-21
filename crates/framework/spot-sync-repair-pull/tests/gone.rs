//! A `Gone` answer resolves a miss: the record was deleted, so it is neither asked again nor kept
//! as missing (WP-SYN-02.T6).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use spot_sync_node::{Position, RecordKey};
use spot_sync_repair_pull::{ConditionClear, Input, Output, RepairConfig, RepairLoop, ScopeCheck};

struct Scope;

impl ScopeCheck for Scope {
    fn in_scope(&self, _tenant: &str, _key: &RecordKey) -> bool {
        true
    }
    fn scope(&self, _tenant: &str) -> String {
        "branch:khi".to_owned()
    }
    fn applied_prefix(&self, _tenant: &str) -> Option<Position> {
        Some(1)
    }
}

fn k(id: &str) -> RecordKey {
    RecordKey::new("Item", id)
}

fn missed(tenant: &str, keys: Vec<RecordKey>, now_ms: u64) -> Input {
    Input::Missed {
        tenant: tenant.to_owned(),
        keys,
        bundle_position: 10,
        now_ms,
    }
}

fn gone(tenant: &str, keys: Vec<RecordKey>) -> Input {
    Input::Gone {
        tenant: tenant.to_owned(),
        keys,
    }
}

fn new_loop() -> RepairLoop {
    RepairLoop::new("n1", RepairConfig::default()).expect("config")
}

#[test]
fn a_gone_key_is_resolved_and_reported_for_the_node() {
    let mut l = new_loop();
    l.step(missed("t1", vec![k("a"), k("b")], 0), &Scope);
    let out = l.step(gone("t1", vec![k("a")]), &Scope);
    assert_eq!(
        out,
        vec![Output::Gone {
            tenant: "t1".to_owned(),
            keys: vec![k("a")],
        }]
    );
    assert_eq!(l.tracked_len(), 1);
    assert_eq!(l.counters().gone, 1);
}

#[test]
fn a_gone_key_is_never_asked_again() {
    let mut l = new_loop();
    l.step(missed("t1", vec![k("a")], 0), &Scope);
    l.step(gone("t1", vec![k("a")]), &Scope);
    let out = l.step(Input::Tick(10_000_000), &Scope);
    assert_eq!(out, vec![Output::Nothing]);
    assert_eq!(l.tracked_len(), 0);
    assert_eq!(l.tenant_count(), 0);
}

#[test]
fn a_gone_answer_for_a_key_nobody_missed_reports_nothing() {
    let mut l = new_loop();
    l.step(missed("t1", vec![k("a")], 0), &Scope);
    let out = l.step(gone("t1", vec![k("zzz")]), &Scope);
    assert_eq!(out, vec![Output::Nothing]);
    assert_eq!(l.tracked_len(), 1);
    // An unknown tenant is not created.
    assert_eq!(
        l.step(gone("other", vec![k("a")]), &Scope),
        vec![Output::Nothing]
    );
    assert_eq!(l.tenant_count(), 1);
}

#[test]
fn the_last_gone_key_clears_the_condition() {
    let mut l = new_loop();
    l.step(missed("t1", vec![k("a")], 0), &Scope);
    let raised = l.step(
        Input::Tick(RepairConfig::default().raise_after_ms + 1),
        &Scope,
    );
    assert!(raised.iter().any(|o| matches!(o, Output::Raise(_))));
    assert!(l.condition_raised("t1"));
    let out = l.step(gone("t1", vec![k("a")]), &Scope);
    assert!(out.contains(&Output::Clear(ConditionClear {
        subject: "node:n1".to_owned(),
        tenant: "t1".to_owned(),
    })));
    assert!(!l.condition_raised("t1"));
}

#[test]
fn a_gone_answer_touches_only_its_own_tenant() {
    let mut l = new_loop();
    l.step(missed("t1", vec![k("a")], 0), &Scope);
    l.step(missed("t2", vec![k("a")], 0), &Scope);
    l.step(gone("t1", vec![k("a")]), &Scope);
    assert_eq!(l.tracked_len_for("t1"), 0);
    assert_eq!(l.tracked_len_for("t2"), 1);
}
