//! Expiry events, exactly once.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::{T0, counting_ids, later, rule_set, settle, users};
use spot_iam_assign::{Assignment, EndCause, EventPayload, Source, expire, plan_rules_guarded};

fn timed(id: &str, source: Source, hours: i64) -> Assignment {
    let mut a = Assignment::new(id, "ben", "Manager");
    a.source = source;
    a.valid_to = Some(later(hours));
    a
}

#[test]
fn each_expired_assignment_is_announced_exactly_once() {
    let current = vec![
        timed("a", Source::Manual, 1),
        timed("b", Source::Emergency, 2),
        timed("c", Source::Manual, 10),
    ];
    let mut announced = BTreeSet::new();
    let first = expire(&current, &announced, later(3));
    assert_eq!(first.ended_marker, ["a", "b"]);
    assert!(first.events.iter().all(|e| matches!(
        e,
        EventPayload::Ended {
            cause: EndCause::Expired,
            ..
        }
    )));
    assert_eq!(first.events[1].topic(), "identity_assignment_ended");
    announced.extend(first.ended_marker);
    assert!(expire(&current, &announced, later(3)).events.is_empty());
    assert!(expire(&current, &announced, later(4)).events.is_empty());
    let third = expire(&current, &announced, later(11));
    assert_eq!(third.ended_marker, ["c"]);
}

#[test]
fn a_rule_assignment_with_a_time_box_expires_too() {
    let mut ids = counting_ids();
    let set = rule_set();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &users(),
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let plan = expire(&state, &BTreeSet::new(), later(25));
    assert_eq!(plan.events.len(), 1);
    assert!(matches!(
        &plan.events[0],
        EventPayload::Ended { role, source: Source::Rule, cause: EndCause::Expired, .. }
            if role.as_str() == "Auditor"
    ));
}

#[test]
fn an_assignment_ended_by_a_plan_is_not_announced_again_as_expired() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut all = users();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    all[1].1 = spot_iam_assign::Facts::new().text("department", "Legal");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    let state = settle(&plan, &state);
    let announced: BTreeSet<String> = plan.ended_marker.iter().cloned().collect();
    assert_eq!(announced.len(), 1);
    let later_plan = expire(&state, &announced, later(2));
    assert!(later_plan.events.is_empty());
}

#[test]
fn the_summary_counts_what_the_plan_does() {
    let mut ids = counting_ids();
    let plan = plan_rules_guarded(
        &rule_set(),
        &mut ids,
        &[],
        &users(),
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let s = plan.summary();
    assert!(s.starts_with("grant 4, end 0, replace 0"), "{s}");
}
