#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Duplicated and reordered traffic (ADR-062 G4, convergence): every frame sent twice, frames
//! shuffled in windows of up to eight, a lossy wire, and a second tenant's records in the same
//! stream. The result must not change: per-key order, no gap acknowledged, nothing applied twice.

use proptest::prelude::*;
use spot_sync_chaos::{World, WorldConfig, assert_guarantees, report_failure};

fn run(cfg: WorldConfig) -> World {
    let mut world = World::build(cfg).unwrap();
    world
        .settle(400)
        .unwrap_or_else(|error| panic!("{}", report_failure(&error)));
    assert_guarantees(&world);
    world
}

#[test]
fn every_message_duplicated_converges_to_the_same_state() {
    for seed in 0..8 {
        let clean = run(WorldConfig {
            nodes: 3,
            ..WorldConfig::seeded(seed)
        });
        let doubled = run(WorldConfig {
            nodes: 3,
            duplicate: true,
            ..WorldConfig::seeded(seed)
        });
        for node in 0..3 {
            assert_eq!(clean.applied(node), doubled.applied(node), "SEED={seed}");
        }
        assert!(
            doubled.store(0).unwrap().repeated().is_empty(),
            "SEED={seed} a duplicate was applied twice"
        );
    }
}

#[test]
fn reorder_windows_up_to_eight_still_converge() {
    for window in 1..=8 {
        for seed in 0..6 {
            run(WorldConfig {
                nodes: 3,
                reorder_window: window,
                batch: 4,
                ..WorldConfig::seeded(seed)
            });
        }
    }
}

#[test]
fn duplicates_reorder_and_loss_together_still_converge() {
    for seed in 0..12 {
        let world = run(WorldConfig {
            nodes: 4,
            reorder_window: 8,
            duplicate: true,
            loss_permille: 150,
            batch: 3,
            ..WorldConfig::seeded(seed)
        });
        assert!(
            world.rounds() > 2,
            "SEED={seed} the faults had nothing to act on"
        );
    }
}

#[test]
fn a_node_with_a_narrow_scope_gets_only_its_entities_under_chaos() {
    for seed in 0..6 {
        let world = run(WorldConfig {
            nodes: 3,
            narrow_node: Some(1),
            narrow_entities: vec!["Item".to_owned()],
            reorder_window: 6,
            duplicate: true,
            ..WorldConfig::seeded(seed)
        });
        assert!(world.applied(1).keys().all(|key| key.entity == "Item"));
        assert!(!world.applied(0).is_empty());
    }
}

#[test]
fn the_other_tenants_records_never_reach_any_node() {
    for seed in 0..8 {
        let world = run(WorldConfig {
            nodes: 3,
            write_foreign: true,
            reorder_window: 8,
            duplicate: true,
            loss_permille: 100,
            ..WorldConfig::seeded(seed)
        });
        assert!(world.tenants().len() >= 2, "no second tenant was written");
        for node in 0..3 {
            assert!(
                world.foreign_records(node).is_empty(),
                "SEED={seed} node-{node} holds another tenant's record"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn any_window_duplication_and_loss_converges(
        seed in 0u64..10_000,
        window in 1usize..=8,
        duplicate in any::<bool>(),
        loss in 0u64..300,
        batch in 1usize..5,
    ) {
        let mut world = World::build(WorldConfig {
            nodes: 2,
            records: 5,
            reorder_window: window,
            duplicate,
            loss_permille: loss,
            batch,
            ..WorldConfig::seeded(seed)
        }).unwrap();
        prop_assert!(world.settle(600).is_ok(), "SEED={}", seed);
        prop_assert!(spot_sync_chaos::check_guarantees(&world, true).is_empty(), "SEED={}", seed);
    }
}
