//! Deployment facts from outside the crate: the epoch a fact is keyed by is the deployment model's,
//! and an older epoch is visible as stale knowledge.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_agent_memory::testing::{embedder, sanitiser};
use spot_agent_memory::{
    Entry, Epoch, Fact, FactId, FactInput, FactKey, FactValue, InMemoryStore, MemoryStore,
    SourceKind, TenantId, Tier,
};
use spot_deployment_model::{
    Backend, BackendEngine, CacheBackend, Connectivity, Coordination, Coordinator, DeploymentNode,
    Edition, Facts, MemoryFacts, NodeRole, OsFamily, Placement, Platform, Replica, ReplicaRole,
    SitePolicies, SyncProfile, Topology, TopologyMode, refresh,
};

/// The deployment ground truth of the platform's own doc example: one standalone node.
fn facts() -> Facts {
    Facts {
        edition: Some(Edition::new("enterprise").unwrap()),
        platform: Some(Platform::new(OsFamily::Linux, "Ubuntu 24.04").unwrap()),
        topology: Some(
            Topology::new(
                TopologyMode::Standalone,
                Placement::HeadOffice,
                BTreeMap::from([(
                    "ln-1".to_owned(),
                    DeploymentNode::new([NodeRole::Web, NodeRole::Database]).unwrap(),
                )]),
            )
            .unwrap(),
        ),
        backend: Some(
            Backend::new(
                BackendEngine::Postgres,
                "16.4",
                vec![Replica::new("ln-1", ReplicaRole::Primary, false).unwrap()],
            )
            .unwrap(),
        ),
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

#[test]
fn a_fact_is_keyed_by_the_deployment_models_own_epoch() {
    let source = MemoryFacts::with_facts(1, facts());
    let model = refresh(None, &[&source], 1_000).expect("the deployment model refreshes");
    let epoch = Epoch::of(&model);
    assert_eq!(epoch.get(), model.epoch);

    let sanitiser = sanitiser();
    let fact = Fact::record(
        &sanitiser,
        &embedder(),
        FactInput {
            id: FactId::parse("fact.backend.postgres").unwrap(),
            key: FactKey::parse("backend.postgres_version").unwrap(),
            value: FactValue::text(&sanitiser, "16.4").unwrap(),
            epoch,
            meta: spot_agent_memory::EntryMeta::new(
                Tier::T1,
                TenantId::parse("tenant_a").unwrap(),
                1_000,
                SourceKind::Deployment,
            )
            .unwrap(),
            text: None,
        },
    )
    .expect("a deployment fact records");

    assert_eq!(fact.epoch, epoch);
    assert!(
        !fact.is_stale_at(epoch),
        "knowledge at the current epoch is current"
    );
    assert!(
        fact.is_stale_at(Epoch::new(model.epoch + 1)),
        "a fact from an older epoch is visible as stale"
    );

    let store = InMemoryStore::new();
    store.put(Entry::Fact(fact)).unwrap();
    assert_eq!(
        store.count(
            &TenantId::parse("tenant_a").unwrap(),
            spot_agent_memory::MemoryKind::Fact
        ),
        1
    );
}
