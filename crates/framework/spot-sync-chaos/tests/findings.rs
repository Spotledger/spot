#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Defects the suite found in the composition of the hub relay and the node intake, kept as tests
//! so they stay visible and so the checker is proved to catch them. Each one is a seed and a
//! configuration; fixing the defect (a separate task) turns the assertion round, and the test
//! should then be changed to expect convergence.
//!
//! Both come from one cause: the node's `applied_prefix` is the highest position it has settled,
//! and it cannot see a bundle it was never sent, while the hub trims its queue by that prefix.
//! Anything that makes a node settle a high position before a lower one is still queued makes
//! the acknowledgement claim more than was applied, and the hub then drops the rest.

use spot_sync_chaos::{Guarantee, World, WorldConfig, check_guarantees};

fn violations(cfg: WorldConfig) -> Vec<Guarantee> {
    let mut world = World::build(cfg).unwrap();
    for _ in 0..8 {
        world.round().unwrap();
    }
    check_guarantees(&world, false)
        .into_iter()
        .map(|v| v.guarantee)
        .collect()
}

#[test]
fn finding_images_that_run_ahead_of_the_stream_make_the_prefix_leap() {
    // Records are updated before the hub bundles their first entry, so the bundle carries the
    // latest image (position 14, say) while entries 5 to 13 are still queued. The node settles
    // 14, acknowledges 14, and the hub trims 5 to 13 unserved.
    let found = violations(WorldConfig {
        nodes: 1,
        images_run_ahead: true,
        ..WorldConfig::seeded(5)
    });
    assert!(
        found.contains(&Guarantee::AcknowledgementsMeanApplied),
        "SEED=5 expected the leap; the defect may be fixed, update this test"
    );
}

#[test]
fn finding_the_mandatory_lane_served_first_makes_the_prefix_leap() {
    // Customers are served before the bulk lane regardless of position, so a node can settle a
    // high Customer position and acknowledge it before a lower SalesInvoice position was sent.
    let found = violations(WorldConfig {
        nodes: 1,
        mandatory_customers: true,
        batch: 2,
        ..WorldConfig::seeded(5)
    });
    assert!(
        found.contains(&Guarantee::AcknowledgementsMeanApplied),
        "SEED=5 expected the leap; the defect may be fixed, update this test"
    );
}

#[test]
fn the_default_world_has_neither_defect() {
    assert!(
        violations(WorldConfig {
            nodes: 1,
            ..WorldConfig::seeded(5)
        })
        .is_empty()
    );
}
