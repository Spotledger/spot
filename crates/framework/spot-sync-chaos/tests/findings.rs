#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Two defects the suite found in the composition of the hub relay and the node intake (PR 2269),
//! and the proof that they are fixed (WP-SYN-11.T2, option A: acknowledge by serve sequence).
//!
//! The cause was one thing: the node acknowledged its highest settled journal position, it cannot
//! see a bundle it was never sent, and the hub trimmed its queue by that position. Anything that
//! made a node settle a high position before a lower one was still queued made the
//! acknowledgement claim more than was applied, and the hub then dropped the rest.
//!
//! The hub's queue now numbers every bundle it serves, each bundle names the one queued below it,
//! the node acknowledges the highest number it has applied everything up to, and the hub trims by
//! that number only and derives the journal prefix itself. Both triggers, at the seed that
//! exposed them, now acknowledge only what was applied, with no fault at all.

use spot_sync_chaos::{
    World, WorldConfig, assert_guarantees, check_guarantees, replay_line, report_failure,
};

/// Run the world, checking the five always-true guarantees after every round (a leap shows at the
/// moment the acknowledgement is sent, not only at the end), then settle and check all six.
fn proves_the_guarantees(cfg: WorldConfig) -> World {
    let seed = cfg.seed;
    let mut world = World::build(cfg).unwrap_or_else(|error| panic!("{}", report_failure(&error)));
    for round in 0..8 {
        world
            .round()
            .unwrap_or_else(|error| panic!("{}", report_failure(&error)));
        let found = check_guarantees(&world, false);
        assert!(
            found.is_empty(),
            "{} round {round}: {found:?}",
            replay_line(seed)
        );
    }
    world
        .settle(400)
        .unwrap_or_else(|error| panic!("{}", report_failure(&error)));
    assert_guarantees(&world);
    world
}

#[test]
fn images_that_run_ahead_of_the_stream_no_longer_make_the_acknowledgement_leap() {
    // Records are updated before the hub bundles their first entry, so the bundle carries the
    // latest image (position 14, say) while entries 5 to 13 are still queued. The node settles
    // position 14 but acknowledges only the serve sequence below which everything is applied,
    // so the hub keeps entries 5 to 13 until they are.
    let world = proves_the_guarantees(WorldConfig {
        nodes: 1,
        images_run_ahead: true,
        ..WorldConfig::seeded(5)
    });
    assert_eq!(world.queued(0), 0, "{}", replay_line(5));
}

#[test]
fn the_mandatory_lane_served_first_no_longer_makes_the_acknowledgement_leap() {
    // Customers are served before the bulk lane regardless of position, so a node settles a high
    // Customer position before a lower SalesInvoice position was sent. It no longer acknowledges
    // that position until the lower bundles are applied.
    let world = proves_the_guarantees(WorldConfig {
        nodes: 1,
        mandatory_customers: true,
        batch: 2,
        ..WorldConfig::seeded(5)
    });
    assert_eq!(world.queued(0), 0, "{}", replay_line(5));
}

#[test]
fn both_triggers_at_once_with_several_lanes_and_nodes_still_acknowledge_only_what_was_applied() {
    let world = proves_the_guarantees(WorldConfig {
        nodes: 3,
        images_run_ahead: true,
        mandatory_customers: true,
        batch: 2,
        ..WorldConfig::seeded(5)
    });
    for node in 0..3 {
        assert_eq!(world.queued(node), 0, "{} node {node}", replay_line(5));
    }
}

#[test]
fn the_default_world_still_has_no_defect() {
    proves_the_guarantees(WorldConfig {
        nodes: 1,
        ..WorldConfig::seeded(5)
    });
}
