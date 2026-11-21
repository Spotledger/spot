//! Shared fixtures for the acceptance tests.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_desired_state::{
    ApprovalRules, BulkPlan, Drift, DriftSubject, Epoch, Fleet, FleetNode, InMemoryObserved,
    NodeId, ObservedSnapshot, OperationSet, ParamValue, Percent, PlanStep, ReleaseId, Risk,
    RunGuards, Seed, Selector, SettingKey, Soak, Wave, WaveSize, operation,
};
use spot_settings::{RegistryBuilder, SettingsRegistry, load_seed_catalogue};

/// The time the fixture reports were received, and the silence deadline tests use.
pub const NOW: i64 = 1_000;
/// A deadline long enough that the fixture reports are fresh in every test.
pub const DEADLINE: i64 = 10_000;

/// The group every fixture belongs to.
pub const GROUP: &str = "prod";

/// A registry with the ten seed settings.
pub fn registry() -> SettingsRegistry {
    RegistryBuilder::new()
        .register_all(load_seed_catalogue().unwrap())
        .build()
        .unwrap()
}

/// A server id.
pub fn server(index: usize) -> NodeId {
    NodeId::new(format!("srv-{index:03}")).unwrap()
}

/// `count` servers.
pub fn servers(count: usize) -> Vec<NodeId> {
    (0..count).map(server).collect()
}

/// A setting key.
pub fn key(text: &str) -> SettingKey {
    SettingKey::new(text).unwrap()
}

/// A group layer for `members` that wants `runtime.logging.level` at `warn`.
pub fn group_state(members: &[NodeId]) -> spot_desired_state::DesiredState {
    let mut state = spot_desired_state::DesiredState::for_group(GROUP).with_setting(
        key("runtime.logging.level"),
        ParamValue::Text("warn".to_owned()),
    );
    for member in members {
        state = state.with_server(member.clone());
    }
    state
}

/// Agent reports for `members`; members whose index is in `drifted` report `debug`.
pub fn observed(members: &[NodeId], drifted: &BTreeSet<usize>) -> InMemoryObserved {
    let mut observed = InMemoryObserved::new();
    for (index, member) in members.iter().enumerate() {
        let level = if drifted.contains(&index) {
            "debug"
        } else {
            "warn"
        };
        observed.record(
            member.clone(),
            ObservedSnapshot::new(1_000).with_setting(
                key("runtime.logging.level"),
                ParamValue::Text(level.to_owned()),
            ),
        );
    }
    observed
}

/// One drift per server, all a logging-level difference from `debug` to `warn`.
pub fn drifts(members: &[NodeId]) -> Vec<Drift> {
    let operations = OperationSet::builtin().unwrap();
    members
        .iter()
        .map(|member| Drift {
            server: member.clone(),
            group: GROUP.to_owned(),
            subject: DriftSubject::Setting(key("runtime.logging.level")),
            risk: Risk::Low,
            desired: Some(ParamValue::Text("warn".to_owned())),
            actual: Some(ParamValue::Text("debug".to_owned())),
            remediation: vec![operation(
                &operations.set_setting,
                [
                    ("key", ParamValue::Text("runtime.logging.level".to_owned())),
                    ("value", ParamValue::Text("warn".to_owned())),
                ],
            )],
        })
        .collect()
}

/// A hand-built one-operation plan for `members`, for tests of the run machinery.
pub fn plan_for(members: &[NodeId]) -> BulkPlan {
    let operations = OperationSet::builtin().unwrap();
    let steps = members
        .iter()
        .enumerate()
        .map(|(index, member)| PlanStep {
            id: format!("{member}/{}", index.saturating_add(1)),
            server: member.clone(),
            group: GROUP.to_owned(),
            operations: vec![operation(
                &operations.set_setting,
                [
                    ("key", ParamValue::Text("runtime.logging.level".to_owned())),
                    ("value", ParamValue::Text("warn".to_owned())),
                ],
            )],
            risk: Risk::Low,
        })
        .collect();
    BulkPlan {
        group: GROUP.to_owned(),
        steps,
        skipped: Vec::new(),
        risk: Risk::Low,
    }
}

/// The fleet of `members`.
pub fn fleet(members: &[NodeId]) -> Fleet {
    Fleet::new(
        members
            .iter()
            .map(|member| FleetNode::new(member.clone(), GROUP))
            .collect(),
    )
}

/// Canary of one, then ten percent of the rest, then the rest.
pub fn waves() -> Vec<Wave> {
    vec![
        Wave::new("canary", Selector::all(), WaveSize::Nodes(1), Soak::ZERO).as_canary(),
        Wave::new(
            "widen",
            Selector::all().with_percentage(Percent::new(10).unwrap()),
            WaveSize::All,
            Soak::ZERO,
        ),
        Wave::new("fleet", Selector::all(), WaveSize::All, Soak::ZERO),
    ]
}

/// the rollout plan for a bulk release.
pub fn rollout_plan() -> spot_desired_state::RolloutPlan {
    spot_desired_state::RolloutPlan::new(
        ReleaseId::new("bulk-2026.10").unwrap(),
        Epoch::new(1),
        waves(),
    )
}

/// The seed the rollout order uses.
pub fn seed() -> Seed {
    Seed::new(7)
}

/// Who asked for the change in the fixtures.
pub const REQUESTER: &str = "requester";

/// The approval rules the fixtures use: a minute old at most, one approver.
pub fn rules() -> ApprovalRules {
    ApprovalRules::new(60_000_000, 1)
}

/// Run guards over fresh reports for `members` at [`NOW`].
pub fn guards(members: &[NodeId]) -> RunGuards {
    RunGuards::new(
        Arc::new(observed(members, &BTreeSet::new())),
        DEADLINE,
        rules(),
    )
}
