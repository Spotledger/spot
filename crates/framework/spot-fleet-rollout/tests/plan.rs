//! The plan resolves a fleet deterministically and honours selectors, sizes and the structural
//! rules.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use spot_fleet_rollout::{
    Epoch, Fleet, FleetNode, Percent, ReleaseId, RolloutPlan, Seed, Selector, Soak, Wave, WaveSize,
};
use support::{canary, fleet, node, plan, wave};

#[test]
fn the_same_seed_gives_the_same_schedule() {
    let plan = plan(vec![
        canary(3, 1),
        wave("middle", 5, 1),
        wave("tail", 20, 0),
    ]);
    let first = plan.resolve(&fleet(30), Seed::new(11)).unwrap();
    let second = plan.resolve(&fleet(30), Seed::new(11)).unwrap();
    assert_eq!(first, second);
}

#[test]
fn a_different_seed_changes_the_order() {
    let plan = plan(vec![canary(5, 1), wave("tail", 20, 0)]);
    let set = |seed| {
        plan.resolve(&fleet(30), Seed::new(seed))
            .unwrap()
            .into_iter()
            .flat_map(|wave| wave.nodes)
            .collect::<Vec<_>>()
    };
    assert_ne!(set(1), set(2));
}

#[test]
fn every_node_is_in_exactly_one_wave() {
    let plan = plan(vec![
        canary(2, 1),
        wave("middle", 10, 1),
        wave("tail", 100, 0),
    ]);
    let resolved = plan.resolve(&fleet(25), Seed::new(4)).unwrap();
    let all: Vec<_> = resolved
        .iter()
        .flat_map(|wave| wave.nodes.clone())
        .collect();
    let unique: BTreeSet<_> = all.iter().cloned().collect();
    assert_eq!(all.len(), 25);
    assert_eq!(unique.len(), 25);
}

#[test]
fn selectors_filter_by_tag_and_group_then_take_a_percentage() {
    let fleet = Fleet::new(vec![
        FleetNode::new(node("north-1"), "north").with_tag("retail"),
        FleetNode::new(node("north-2"), "north").with_tag("retail"),
        FleetNode::new(node("north-3"), "north").with_tag("retail"),
        FleetNode::new(node("south-1"), "south").with_tag("retail"),
        FleetNode::new(node("south-2"), "south").with_tag("warehouse"),
    ]);
    let selector = Selector::all()
        .with_tags(["retail"])
        .with_groups(["north"])
        .with_percentage(Percent::new(34).unwrap());
    let plan = RolloutPlan::new(
        ReleaseId::new("r").unwrap(),
        Epoch::new(1),
        vec![Wave::new("canary", selector, WaveSize::All, Soak::ZERO).as_canary()],
    );
    let resolved = plan.resolve(&fleet, Seed::new(2)).unwrap();
    assert_eq!(resolved[0].nodes.len(), 1);
    assert!(resolved[0].nodes[0].as_str().starts_with("north-"));
}

#[test]
fn wave_size_limits_the_selection() {
    let selector = Selector::all().with_percentage(Percent::new(50).unwrap());
    let plan = RolloutPlan::new(
        ReleaseId::new("r").unwrap(),
        Epoch::new(1),
        vec![
            Wave::new("canary", selector, WaveSize::Nodes(2), Soak::ZERO).as_canary(),
            Wave::new("rest", Selector::all(), WaveSize::All, Soak::ZERO),
        ],
    );
    let resolved = plan.resolve(&fleet(10), Seed::new(1)).unwrap();
    assert_eq!(resolved[0].nodes.len(), 2);
    assert_eq!(resolved[1].nodes.len(), 8);
}

#[test]
fn an_empty_wave_is_refused() {
    let mut plan = plan(vec![canary(1, 0)]);
    plan.waves.push(Wave::new(
        "empty",
        Selector::all().with_groups(["missing"]),
        WaveSize::All,
        Soak::ZERO,
    ));
    let error = plan.resolve(&fleet(2), Seed::new(1)).unwrap_err();
    assert_eq!(error.code(), "fleet_rollout.empty_wave");
}

#[test]
fn structural_rules_are_reported_with_the_invalid_plan_code() {
    let empty = RolloutPlan::new(ReleaseId::new("r").unwrap(), Epoch::new(1), Vec::new());
    assert_eq!(
        empty.validate().unwrap_err().code(),
        "fleet_rollout.invalid_plan"
    );

    let no_canary = RolloutPlan::new(
        ReleaseId::new("r").unwrap(),
        Epoch::new(1),
        vec![wave("first", 1, 0)],
    );
    assert_eq!(
        no_canary.validate().unwrap_err().code(),
        "fleet_rollout.invalid_plan"
    );

    let duplicated = RolloutPlan::new(
        ReleaseId::new("r").unwrap(),
        Epoch::new(1),
        vec![canary(1, 0), wave("canary", 1, 0)],
    );
    assert_eq!(
        duplicated.validate().unwrap_err().code(),
        "fleet_rollout.invalid_plan"
    );
}
