#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A cut at every protocol step (ADR-067 section 7): the link dies before and after each of the
//! offer, batch, ack, checkpoint and nack, for one node. After the reconnect the world converges
//! and nothing is applied twice.

use spot_sync_chaos::{Edge, Step, World, WorldConfig, assert_guarantees, report_failure};

const SEEDS: [u64; 4] = [1, 2, 3, 17];

fn cfg(seed: u64) -> WorldConfig {
    WorldConfig {
        nodes: 1,
        records: 6,
        versions: 2,
        batch: 2,
        group_max: 2,
        ..WorldConfig::seeded(seed)
    }
}

/// Run until the armed cut has fired (or give up), reconnect, settle and check.
fn cut_and_recover(seed: u64, step: Step, edge: Edge) -> World {
    let mut world = World::build(cfg(seed)).unwrap();
    // A refusal is how a nack comes to exist.
    if step == Step::Nack {
        let key = world.expected().keys().next().cloned().unwrap();
        world.refuse_once(0, &key).unwrap();
    }
    // Let the protocol get going so every step has happened at least once before the cut.
    world.round().unwrap();
    world.cut(0, step, edge);
    for _ in 0..60 {
        if world.cut_fired(0, step) {
            break;
        }
        world.round().unwrap();
    }
    assert!(
        world.cut_fired(0, step),
        "{} the cut at {} {} never fired",
        spot_sync_chaos::replay_line(seed),
        edge.name(),
        step.name()
    );
    world.heal(0);
    world
        .settle(200)
        .unwrap_or_else(|error| panic!("{}", report_failure(&error)));
    assert_guarantees(&world);
    world
}

#[test]
fn a_cut_before_and_after_every_step_still_converges() {
    for seed in SEEDS {
        for step in Step::all() {
            for edge in [Edge::Before, Edge::After] {
                let world = cut_and_recover(seed, step, edge);
                assert!(
                    world.store(0).unwrap().repeated().is_empty(),
                    "SEED={seed} {} {} applied something twice",
                    edge.name(),
                    step.name()
                );
            }
        }
    }
}

#[test]
fn a_cut_after_the_batch_re_delivers_and_the_replay_is_recognised() {
    let world = cut_and_recover(5, Step::Batch, Edge::After);
    let store = world.store(0).unwrap();
    assert!(
        store.duplicates() > 0 || store.applies() > 0,
        "the node applied nothing"
    );
    assert!(store.repeated().is_empty());
}

#[test]
fn a_cut_after_the_checkpoint_loses_only_volatile_state() {
    let world = cut_and_recover(6, Step::Checkpoint, Edge::After);
    // The node restarted: what it had applied was durable, so it is still held.
    assert_eq!(world.applied(0), world.expected());
}

#[test]
fn a_refusal_is_reported_and_then_retried_to_success() {
    let world = cut_and_recover(7, Step::Nack, Edge::Before);
    // The cut swallowed the first nack; the sink recovered, so the retry succeeded.
    assert_eq!(world.applied(0), world.expected());
    assert!(world.store(0).unwrap().refusals() >= 1);
}

#[test]
fn several_cuts_in_one_run_and_two_nodes_do_not_interfere() {
    let mut world = World::build(WorldConfig {
        nodes: 2,
        ..cfg(11)
    })
    .unwrap();
    world.cut(0, Step::Batch, Edge::After);
    world.cut(1, Step::Ack, Edge::Before);
    world.cut(1, Step::Offer, Edge::Before);
    for _ in 0..10 {
        world.round().unwrap();
    }
    world.heal(0);
    world.heal(1);
    world.settle(200).unwrap();
    assert_guarantees(&world);
}
