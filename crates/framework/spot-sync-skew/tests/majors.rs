//! Tests for majors.
mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{decodes, report};
use proptest::prelude::*;
use spot_sync_skew::{
    NodeState, NodeStatus, Retirement, SCHEMA_RETIRE_BLOCKED, Upward, accepts_upward, can_retire,
    served_majors,
};

fn publishes(topic: &str, majors: &[u32]) -> BTreeMap<String, BTreeSet<u32>> {
    BTreeMap::from([(topic.to_string(), majors.iter().copied().collect())])
}

fn node(id: &str, status: NodeStatus, majors: &[u32]) -> NodeState {
    NodeState {
        node: id.to_string(),
        status,
        report: decodes(report(5), "invoice", majors),
    }
}

#[test]
fn each_node_gets_the_major_it_can_read() {
    let p = publishes("invoice", &[1, 2]);
    assert_eq!(
        served_majors(&p, &decodes(report(5), "invoice", &[1])).get("invoice"),
        Some(&1)
    );
    assert_eq!(
        served_majors(&p, &decodes(report(5), "invoice", &[1, 2])).get("invoice"),
        Some(&2)
    );
    assert!(served_majors(&p, &decodes(report(5), "invoice", &[3])).is_empty());
    assert!(served_majors(&p, &report(5)).is_empty());
}

#[test]
fn retirement_blocked_by_one_old_node_then_allowed_after_suspending_it() {
    let mut fleet = vec![
        node("b-new", NodeStatus::Active, &[1, 2]),
        node("a-old", NodeStatus::Active, &[1]),
    ];
    let blocked = can_retire("invoice", 1, &fleet);
    assert_eq!(
        blocked,
        Retirement::Blocked {
            nodes_in_the_way: vec!["a-old".to_string()]
        }
    );
    assert_eq!(blocked.condition_code(), Some(SCHEMA_RETIRE_BLOCKED));
    fleet[1].status = NodeStatus::Suspended;
    let allowed = can_retire("invoice", 1, &fleet);
    assert_eq!(allowed, Retirement::Allowed);
    assert_eq!(allowed.condition_code(), None);
    assert_eq!(can_retire("invoice", 1, &[]), Retirement::Allowed);
}

#[test]
fn upward_accepts_only_the_previous_major() {
    assert_eq!(accepts_upward(2, 2), Upward::Accepted);
    assert_eq!(
        accepts_upward(2, 1),
        Upward::UpcastRequired { from: 1, to: 2 }
    );
    assert_eq!(accepts_upward(3, 1), Upward::Refused);
    assert_eq!(accepts_upward(2, 3), Upward::Refused);
    assert_eq!(accepts_upward(0, u32::MAX), Upward::Refused);
}

proptest! {
    #[test]
    fn served_major_is_always_decodable_and_published(
        published in proptest::collection::btree_map("[a-c]", proptest::collection::btree_set(1u32..5, 0..4), 0..4),
        known in proptest::collection::btree_map("[a-c]", proptest::collection::btree_set(1u32..5, 0..4), 0..4),
    ) {
        let mut n = report(5);
        n.decodable_majors = known;
        let served = served_majors(&published, &n);
        for (topic, major) in &served {
            prop_assert!(n.can_decode(topic, *major));
            prop_assert!(published.get(topic).is_some_and(|s| s.contains(major)));
        }
        // and nothing decodable is left unserved
        for (topic, majors) in &published {
            if majors.iter().any(|m| n.can_decode(topic, *m)) {
                prop_assert!(served.contains_key(topic));
            }
        }
    }
}
