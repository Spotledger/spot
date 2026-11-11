//! Fixtures shared by the integration tests.
//!
//! `base_facts` is a realistic fresh installation: one Linux node running web and the
//! database, PostgreSQL 16, the platform's own cache, no sync, no apps, no migrations.

#![allow(dead_code)]

use std::collections::BTreeMap;

use spot_agent_tiers::Tier;
use spot_deployment_model::{
    AppEntry, AppState, ApplyOutcome, Backend, BackendEngine, CacheBackend, Connectivity,
    Coordination, Coordinator, DeploymentEvent, DeploymentEventKind, DeploymentModel,
    DeploymentNode, Edition, EvidenceRef, EvidenceSource, Facts, MigrationRun, NodeRole, OsFamily,
    Placement, Platform, Replica, ReplicaRole, SitePolicies, SyncProfile, Topology, TopologyMode,
};

/// A recorded event's evidence pointer.
pub fn evidence(id: &str, at: i64) -> EvidenceRef {
    EvidenceRef::at(EvidenceSource::Event, id, at)
}

/// A deployment event with evidence.
pub fn event(id: &str, epoch: u64, at: i64, kind: DeploymentEventKind) -> DeploymentEvent {
    DeploymentEvent::new(id, epoch, at, evidence(id, at), kind).expect("the test event is valid")
}

/// A node with these roles.
pub fn node(roles: &[NodeRole]) -> DeploymentNode {
    DeploymentNode::new(roles.iter().copied()).expect("the test node has roles")
}

/// A standalone, head-office topology with one node.
pub fn standalone_topology() -> Topology {
    Topology::new(
        TopologyMode::Standalone,
        Placement::HeadOffice,
        BTreeMap::from([(
            "ln-1".to_owned(),
            node(&[NodeRole::Web, NodeRole::Database]),
        )]),
    )
    .expect("the test topology is valid")
}

/// A backend with one primary.
pub fn single_primary() -> Backend {
    Backend::new(
        BackendEngine::Postgres,
        "16.4",
        vec![Replica::new("ln-1", ReplicaRole::Primary, false).expect("the replica is valid")],
    )
    .expect("the test backend is valid")
}

/// A fresh installation's complete facts.
pub fn base_facts() -> Facts {
    Facts {
        edition: Some(Edition::new("enterprise").expect("the edition is valid")),
        platform: Some(Platform::new(OsFamily::Linux, "Ubuntu 24.04").expect("the platform")),
        topology: Some(standalone_topology()),
        backend: Some(single_primary()),
        coordination: Some(Coordination::new(
            CacheBackend::Database,
            Coordinator::InProcess,
        )),
        sync: Some(SyncProfile::standalone()),
        runtime_version: Some("0.1.0".to_owned()),
        apps: Some(BTreeMap::new()),
        overrides: Some(Vec::new()),
        migrations: Some(Vec::new()),
        connectivity: Some(Connectivity::new(true, Some(1_000_000))),
        policies: Some(SitePolicies::default()),
        agent_tier: Some(Tier::T1),
    }
}

/// One installed app entry.
pub fn app(version: &str) -> AppEntry {
    AppEntry::new(version, AppState::Installed).expect("the app is valid")
}

/// One migration run.
pub fn migration(run: &str, at: i64) -> MigrationRun {
    MigrationRun::new(run, "applied", at).expect("the migration run is valid")
}

/// The model an applied outcome carries, or a panic naming the outcome.
pub fn applied(outcome: ApplyOutcome) -> DeploymentModel {
    match outcome {
        ApplyOutcome::Applied { model, .. } => *model,
        other => panic!("expected the event to apply, got {other:?}"),
    }
}
