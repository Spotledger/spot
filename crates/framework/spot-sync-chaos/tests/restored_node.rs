#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A node restored from an older backup (ADR-066 section 4): the hub must notice and bootstrap
//! it from a snapshot instead of continuing from a position that belongs to a history that no
//! longer exists.

use spot_sync_bootstrap::{NodeState, StartMode, StartReason};
use spot_sync_chaos::{World, WorldConfig, assert_guarantees};

fn settled(seed: u64) -> World {
    let mut world = World::build(WorldConfig {
        nodes: 3,
        records: 8,
        versions: 3,
        ..WorldConfig::seeded(seed)
    })
    .unwrap();
    world.settle(300).unwrap();
    assert_guarantees(&world);
    world
}

#[test]
fn a_restored_node_is_bootstrapped_not_continued() {
    for seed in 0..10 {
        let mut world = settled(seed);
        let head = world.head();
        // A seeded point in the past.
        let position = world.draw_below(head / 2) + 1;
        world.restore_node(1, position).unwrap();
        assert!(
            !world.state_matches(1),
            "SEED={seed} the restore changed nothing"
        );

        let report = world.start_session(1).unwrap();
        assert!(
            matches!(
                report.mode,
                StartMode::Bootstrap {
                    reason: StartReason::StaleGeneration { .. },
                    ..
                }
            ),
            "SEED={seed} the hub did not detect the restore: {:?}",
            report.mode
        );
        assert!(report.bootstrapped && report.chunks > 0 && report.installed > 0);
        assert!(
            world.state_matches(1),
            "SEED={seed} the bootstrap left a gap"
        );
        world.settle(100).unwrap();
        assert_guarantees(&world);
        // The healthy nodes were never bootstrapped.
        let other = world.start_session(0).unwrap();
        assert!(
            !other.bootstrapped,
            "SEED={seed} a healthy node was bootstrapped"
        );
        assert!(matches!(other.mode, StartMode::Incremental { .. }));
    }
}

#[test]
fn continuing_a_restored_node_would_leave_it_short_for_ever() {
    // The reason the rule exists: the hub already removed what it served from its queue, so
    // nothing refills a node that was rolled back.
    let mut world = settled(3);
    let position = world.head() / 2;
    world.restore_node(1, position).unwrap();
    for _ in 0..40 {
        world.round().unwrap();
    }
    assert!(
        !world.state_matches(1),
        "the node healed without a bootstrap; the restored-node rule would be unnecessary"
    );
}

#[test]
fn the_decision_table_catches_every_way_a_history_can_differ() {
    let world = settled(4);
    let head = world.head();
    let base = NodeState {
        node_id: "node-0".to_owned(),
        tenant: world.tenant().to_owned(),
        generation: world.generation(),
        applied_position: Some(head),
    };
    // A node at the head of the same history continues.
    assert!(matches!(
        world.decide(&base),
        StartMode::Incremental { from } if from == head + 1
    ));
    // A node past the hub's head belongs to another history.
    let ahead = NodeState {
        applied_position: Some(head + 5),
        ..base.clone()
    };
    assert!(matches!(
        world.decide(&ahead),
        StartMode::Bootstrap {
            reason: StartReason::PositionAheadOfHub { .. },
            ..
        }
    ));
    // A node claiming a generation the hub does not have.
    let future = NodeState {
        generation: world.generation() + 1,
        ..base.clone()
    };
    assert!(matches!(
        world.decide(&future),
        StartMode::Bootstrap {
            reason: StartReason::GenerationAheadOfHub { .. },
            ..
        }
    ));
    // A node with nothing is new.
    let blank = NodeState {
        applied_position: None,
        ..base
    };
    assert!(matches!(
        world.decide(&blank),
        StartMode::Bootstrap {
            reason: StartReason::NewNode,
            ..
        }
    ));
}

#[test]
fn a_bootstrapped_node_keeps_receiving_new_versions() {
    let mut world = World::build(WorldConfig {
        nodes: 2,
        records: 6,
        versions: 4,
        ..WorldConfig::seeded(8)
    })
    .unwrap();
    // Run partway: two waves written, the rest still to come.
    for _ in 0..4 {
        world.round().unwrap();
    }
    world.restore_node(0, 3).unwrap();
    let report = world.start_session(0).unwrap();
    assert!(report.bootstrapped);
    world.settle(300).unwrap();
    assert_guarantees(&world);
}
