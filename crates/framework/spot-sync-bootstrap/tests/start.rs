#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The restored-node rule as a table.

use spot_sync_bootstrap::{HubHead, NodeState, StartMode, StartReason, decide_start};

fn node(generation: u64, applied: Option<u64>) -> NodeState {
    NodeState {
        node_id: "n".into(),
        tenant: "t".into(),
        generation,
        applied_position: applied,
    }
}

const HUB: HubHead = HubHead {
    generation: 5,
    head_position: 900,
};

#[test]
fn the_start_table() {
    let snaps = [100, 500, 800, 950];
    let cases: Vec<(NodeState, &[u64], StartMode)> = vec![
        // Same generation, behind the head: continue.
        (
            node(5, Some(400)),
            &snaps,
            StartMode::Incremental { from: 401 },
        ),
        // Exactly at the head: still a continue (nothing to fetch).
        (
            node(5, Some(900)),
            &snaps,
            StartMode::Incremental { from: 901 },
        ),
        // Nothing at all: new node, newest snapshot at or below the head (950 is above it).
        (
            node(0, None),
            &snaps,
            StartMode::Bootstrap {
                snapshot_position: 800,
                reason: StartReason::NewNode,
            },
        ),
        // Restored from an old backup: older generation.
        (
            node(4, Some(400)),
            &snaps,
            StartMode::Bootstrap {
                snapshot_position: 800,
                reason: StartReason::StaleGeneration { node: 4, hub: 5 },
            },
        ),
        // Even a position that looks fine is not trusted under a stale generation.
        (
            node(1, Some(10)),
            &snaps,
            StartMode::Bootstrap {
                snapshot_position: 800,
                reason: StartReason::StaleGeneration { node: 1, hub: 5 },
            },
        ),
        // Node ahead of the hub (hub rolled back).
        (
            node(5, Some(901)),
            &snaps,
            StartMode::Bootstrap {
                snapshot_position: 800,
                reason: StartReason::PositionAheadOfHub {
                    node: 901,
                    hub: 900,
                },
            },
        ),
        // Node on a generation the hub does not have.
        (
            node(6, Some(10)),
            &snaps,
            StartMode::Bootstrap {
                snapshot_position: 800,
                reason: StartReason::GenerationAheadOfHub { node: 6, hub: 5 },
            },
        ),
        // A bootstrap is needed but only a snapshot beyond the head exists.
        (
            node(4, Some(400)),
            &[950],
            StartMode::NeedsSnapshot {
                reason: StartReason::StaleGeneration { node: 4, hub: 5 },
            },
        ),
        (
            node(0, None),
            &[],
            StartMode::NeedsSnapshot {
                reason: StartReason::NewNode,
            },
        ),
        // A position that cannot advance is never continued.
        (
            node(5, Some(u64::MAX)),
            &snaps,
            StartMode::Bootstrap {
                snapshot_position: 800,
                reason: StartReason::PositionAheadOfHub {
                    node: u64::MAX,
                    hub: 900,
                },
            },
        ),
    ];
    for (n, s, want) in cases {
        assert_eq!(decide_start(&n, HUB, s), want, "{n:?}");
    }
}

#[test]
fn a_restored_node_is_never_continued() {
    for generation in 0..10_u64 {
        for applied in [None, Some(0), Some(450), Some(900), Some(1_000)] {
            let mode = decide_start(&node(generation, applied), HUB, &[800]);
            let continues = matches!(mode, StartMode::Incremental { .. });
            let safe = generation == 5 && applied.is_some_and(|p| p <= 900);
            assert_eq!(
                continues, safe,
                "generation {generation} applied {applied:?}"
            );
        }
    }
}
