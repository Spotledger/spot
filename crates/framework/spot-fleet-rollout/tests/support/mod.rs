//! Helpers shared by the integration tests. Test code may unwrap what it built itself.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use spot_fleet_rollout::{
    Epoch, Fleet, FleetNode, NodeId, ReleaseId, RolloutPlan, Selector, Soak, Wave, WaveSize,
};

/// A valid node id.
pub fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("a valid test node")
}

/// A valid release id.
pub fn release() -> ReleaseId {
    ReleaseId::new("2026.10.1").expect("a valid test release")
}

/// A fleet of `count` nodes, each in group `g`.
pub fn fleet(count: usize) -> Fleet {
    Fleet::new(
        (0..count)
            .map(|index| {
                let id = node(&format!("node-{index:03}"));
                FleetNode::new(id, "g")
            })
            .collect(),
    )
}

/// A plan at epoch 5 with the given waves.
pub fn plan(waves: Vec<Wave>) -> RolloutPlan {
    RolloutPlan::new(release(), Epoch::new(5), waves)
}

/// The canary wave taking `size` nodes and soaking for `soak_secs`.
pub fn canary(size: u32, soak_secs: u64) -> Wave {
    Wave::new(
        "canary",
        Selector::all(),
        WaveSize::Nodes(size),
        Soak::from_secs(soak_secs),
    )
    .as_canary()
}

/// A non-canary wave taking `size` nodes and soaking for `soak_secs`.
pub fn wave(name: &str, size: u32, soak_secs: u64) -> Wave {
    Wave::new(
        name,
        Selector::all(),
        WaveSize::Nodes(size),
        Soak::from_secs(soak_secs),
    )
}
