//! The acceptance scenario: migrate, install an app, add a node, promote a replica.
//!
//! After every event the model equals the ground truth, the epoch moved by exactly one, and the
//! digest commits to the new facts. The same events, shuffled and duplicated, converge on the
//! same model — and a resolved incident moves the epoch without inventing a fact change.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::{app, applied, base_facts, event, evidence, migration, node};
use spot_deployment_model::{
    AppState, ApplyOutcome, Backend, ConfigChange, Connectivity, DeploymentEventKind,
    DeploymentModel, EvidenceRef, EvidenceSource, MemoryFacts, NodeRole, Replica, ReplicaRole,
    TopologyMode, apply, refresh, verify,
};

fn fresh_model() -> DeploymentModel {
    let source = MemoryFacts::with_facts(1, base_facts());
    refresh(None, &[&source], 1_000).expect("the fresh model builds")
}

#[test]
fn migration_app_node_and_replica_promotion_match_reality() {
    let mut reality = base_facts();
    let mut model = fresh_model();
    let mut digests = vec![model.digest().expect("the fresh digest")];

    // 1. A migration is applied.
    let run = migration("mig-2026-10-20", 2_000);
    reality.migrations = Some(vec![run.clone()]);
    let migration_event = event(
        "evt-migration",
        2,
        2_000,
        DeploymentEventKind::MigrationApplied {
            run: run.run.clone(),
            outcome: run.outcome.clone(),
        },
    );
    model = applied(apply(&model, &migration_event).expect("the migration applies"));
    assert_eq!(model.epoch, 2);
    assert_eq!(model.last_migration(), Some(&run));
    assert_eq!(
        model.ledger.last().and_then(|entry| entry.evidence.clone()),
        Some(evidence("evt-migration", 2_000)),
        "the epoch names the record that proves it"
    );
    assert!(
        verify(&model, &reality).is_empty(),
        "{:?}",
        verify(&model, &reality)
    );
    digests.push(model.digest().expect("the digest"));

    // 2. An app is installed.
    let selling = app("1.2.0");
    reality
        .apps
        .get_or_insert_with(BTreeMap::new)
        .insert("erp_selling".to_owned(), selling.clone());
    let install = event(
        "evt-install",
        3,
        3_000,
        DeploymentEventKind::AppInstalled {
            app: "erp_selling".to_owned(),
            version: "1.2.0".to_owned(),
        },
    );
    model = applied(apply(&model, &install).expect("the install applies"));
    assert_eq!(model.epoch, 3);
    assert_eq!(model.app("erp_selling"), Some(&selling));
    assert!(verify(&model, &reality).is_empty());
    digests.push(model.digest().expect("the digest"));

    // 3. A second node joins; the topology becomes a cluster.
    let cluster = spot_deployment_model::Topology::new(
        TopologyMode::Cluster,
        spot_deployment_model::Placement::HeadOffice,
        BTreeMap::from([
            (
                "ln-1".to_owned(),
                node(&[NodeRole::Web, NodeRole::Database]),
            ),
            ("ln-2".to_owned(), node(&[NodeRole::Web])),
        ]),
    )
    .expect("the cluster is valid");
    reality.topology = Some(cluster);
    let joined = event(
        "evt-node-joined",
        4,
        4_000,
        DeploymentEventKind::NodeJoined {
            node: "ln-2".to_owned(),
            roles: [NodeRole::Web].into_iter().collect(),
        },
    );
    model = applied(apply(&model, &joined).expect("the node joins"));
    assert_eq!(model.epoch, 4);
    assert_eq!(model.topology.mode, TopologyMode::Cluster);
    assert!(model.node("ln-2").is_some());
    assert!(verify(&model, &reality).is_empty());

    // 4. The standby is promoted: ln-2 is the primary and ln-1 streams from it.
    let promoted = Backend::new(
        spot_deployment_model::BackendEngine::Postgres,
        "16.4",
        vec![
            Replica::new("ln-1", ReplicaRole::Standby, true).expect("the replica is valid"),
            Replica::new("ln-2", ReplicaRole::Primary, false).expect("the replica is valid"),
        ],
    )
    .expect("the promoted backend is valid");
    reality.backend = Some(promoted.clone());
    let promotion = event(
        "evt-promote",
        5,
        5_000,
        DeploymentEventKind::ConfigChanged {
            change: ConfigChange::Backend(promoted),
        },
    );
    model = applied(apply(&model, &promotion).expect("the promotion applies"));
    assert_eq!(model.epoch, 5);
    assert_eq!(
        model.backend.primary().map(|replica| replica.node.as_str()),
        Some("ln-2")
    );
    assert_eq!(model.backend.standby_count(), 1);
    assert!(verify(&model, &reality).is_empty());
    digests.push(model.digest().expect("the digest"));

    // Every fact change moved the digest; the epoch ledger records every event.
    digests.dedup();
    assert_eq!(digests.len(), 4, "each fact change has its own digest");
    assert_eq!(model.ledger.len(), 5, "the refresh and the four events");
    assert_eq!(model.ledger.last().map(|entry| entry.epoch), Some(5));
}

#[test]
fn duplicate_and_stale_events_are_ignored() {
    let model = fresh_model();
    let first = event(
        "evt-1",
        2,
        2_000,
        DeploymentEventKind::AppInstalled {
            app: "erp_selling".to_owned(),
            version: "1.0.0".to_owned(),
        },
    );
    let applied_model = applied(apply(&model, &first).expect("the install applies"));

    let duplicate = apply(&applied_model, &first).expect("a duplicate is not an error");
    assert_eq!(
        duplicate,
        ApplyOutcome::Duplicate { epoch: 2 },
        "the event id is the idempotency key"
    );

    // A different event claiming the same epoch is a conflict, not a silent overwrite.
    let conflicting = event(
        "evt-other",
        2,
        2_100,
        DeploymentEventKind::AppInstalled {
            app: "erp_buying".to_owned(),
            version: "1.0.0".to_owned(),
        },
    );
    assert_eq!(
        apply(&applied_model, &conflicting).expect("a conflict is not an error"),
        ApplyOutcome::Conflict {
            epoch: 2,
            applied: "evt-1".to_owned(),
        }
    );

    // A redelivery of the *previous* epoch is stale.
    let stale = event(
        "evt-0",
        1,
        1_500,
        DeploymentEventKind::IncidentResolved {
            incident: "inc-1".to_owned(),
            detail: "the queue drained".to_owned(),
        },
    );
    assert_eq!(
        apply(&applied_model, &stale).expect("a stale event is not an error"),
        ApplyOutcome::Stale {
            model_epoch: 2,
            event_epoch: 1,
        }
    );
}

#[test]
fn a_gap_is_reported_and_refresh_repairs_it() {
    let model = fresh_model();
    let skipped = event(
        "evt-skip",
        4,
        4_000,
        DeploymentEventKind::AppInstalled {
            app: "erp_selling".to_owned(),
            version: "1.0.0".to_owned(),
        },
    );
    assert_eq!(
        apply(&model, &skipped).expect("a gap is not an error"),
        ApplyOutcome::Gap {
            expected: 2,
            received: 4,
        }
    );

    // The deployment did install the app while the agent was disconnected: a refresh sees it.
    let mut reality = base_facts();
    reality
        .apps
        .get_or_insert_with(BTreeMap::new)
        .insert("erp_selling".to_owned(), app("1.0.0"));
    let source = MemoryFacts::with_facts(9, reality.clone());
    let refreshed = refresh(Some(&model), &[&source], 4_100).expect("the refresh builds");
    assert_eq!(refreshed.epoch, 2);
    assert_eq!(
        refreshed.app("erp_selling").map(|app| app.version.as_str()),
        Some("1.0.0")
    );
    assert!(verify(&refreshed, &reality).is_empty());
    assert_eq!(
        refreshed
            .sources
            .get(&spot_deployment_model::FactSourceKind::Memory),
        Some(&9),
        "the fact carries the source's version"
    );

    // The refresh moved the model to epoch 2, so the event that named epoch 4 still skips
    // epoch 3: a gap again, never a guess.
    assert_eq!(
        apply(&refreshed, &skipped).expect("a gap is not an error"),
        ApplyOutcome::Gap {
            expected: 3,
            received: 4,
        }
    );

    // A second refresh (the deployment has not changed again) lands on epoch 3, and the
    // event's epoch is now exactly the next one: it applies.
    let second = refresh(Some(&refreshed), &[&source], 4_200).expect("the second refresh builds");
    assert_eq!(second.epoch, 3);
    let third = applied(apply(&second, &skipped).expect("the event applies"));
    assert_eq!(third.epoch, 4);
    assert_eq!(
        third.app("erp_selling").map(|app| app.version.as_str()),
        Some("1.0.0")
    );
}

#[test]
fn an_incident_moves_the_epoch_without_inventing_a_fact() {
    let model = fresh_model();
    let before = model.digest().expect("the digest");
    let resolved = event(
        "evt-incident",
        2,
        2_000,
        DeploymentEventKind::IncidentResolved {
            incident: "inc-17".to_owned(),
            detail: "replication caught up".to_owned(),
        },
    );
    let next = applied(apply(&model, &resolved).expect("the resolution applies"));
    assert_eq!(next.epoch, 2);
    assert_eq!(next.digest().expect("the digest"), before);
    assert_eq!(
        next.ledger.last().map(|entry| entry.subject.as_str()),
        Some("incident:inc-17")
    );
    assert_eq!(next.facts(), model.facts());
}

#[test]
fn app_upgrade_and_removal_change_the_facts() {
    let mut model = fresh_model();
    let install = event(
        "evt-install",
        2,
        2_000,
        DeploymentEventKind::AppInstalled {
            app: "erp_selling".to_owned(),
            version: "1.0.0".to_owned(),
        },
    );
    model = applied(apply(&model, &install).expect("the install applies"));

    let upgrade = event(
        "evt-upgrade",
        3,
        3_000,
        DeploymentEventKind::AppUpgraded {
            app: "erp_selling".to_owned(),
            from: "1.0.0".to_owned(),
            to: "1.1.0".to_owned(),
        },
    );
    model = applied(apply(&model, &upgrade).expect("the upgrade applies"));
    assert_eq!(
        model.app("erp_selling").map(|app| app.version.as_str()),
        Some("1.1.0")
    );
    assert_eq!(
        model.app("erp_selling").map(|app| app.state),
        Some(AppState::Installed)
    );

    // An upgrade that does not match the installed version is refused, not guessed.
    let wrong_upgrade = event(
        "evt-wrong",
        4,
        4_000,
        DeploymentEventKind::AppUpgraded {
            app: "erp_selling".to_owned(),
            from: "2.0.0".to_owned(),
            to: "2.1.0".to_owned(),
        },
    );
    assert!(apply(&model, &wrong_upgrade).is_err());
    assert_eq!(model.epoch, 3, "a refused event leaves the model untouched");

    let remove = event(
        "evt-remove",
        4,
        4_500,
        DeploymentEventKind::AppRemoved {
            app: "erp_selling".to_owned(),
        },
    );
    model = applied(apply(&model, &remove).expect("the removal applies"));
    assert!(model.app("erp_selling").is_none());

    // Removing it again cannot apply: the model would have to invent an app.
    let again = event(
        "evt-remove-again",
        5,
        5_000,
        DeploymentEventKind::AppRemoved {
            app: "erp_selling".to_owned(),
        },
    );
    assert!(apply(&model, &again).is_err());
}

#[test]
fn connectivity_and_topology_changes_are_facts() {
    let mut model = fresh_model();
    let offline = event(
        "evt-offline",
        2,
        2_000,
        DeploymentEventKind::ConnectivityChanged {
            connectivity: Connectivity::new(false, Some(1_000_000)),
        },
    );
    model = applied(apply(&model, &offline).expect("the connectivity change applies"));
    assert!(!model.connectivity.hub_reachable);

    let topology = spot_deployment_model::Topology::new(
        TopologyMode::Standalone,
        spot_deployment_model::Placement::BranchEdge,
        BTreeMap::from([(
            "branch-1".to_owned(),
            node(&[NodeRole::Web, NodeRole::BranchEdge]),
        )]),
    )
    .expect("the branch topology is valid");
    let moved = event(
        "evt-topology",
        3,
        3_000,
        DeploymentEventKind::TopologyChanged {
            topology: topology.clone(),
        },
    );
    model = applied(apply(&model, &moved).expect("the topology change applies"));
    assert_eq!(model.topology, topology);

    // Reality agrees.
    let mut reality = base_facts();
    reality.topology = Some(model.topology.clone());
    reality.connectivity = Some(model.connectivity.clone());
    assert!(verify(&model, &reality).is_empty());
}

#[test]
fn an_event_without_an_id_an_epoch_or_evidence_is_refused() {
    let model = fresh_model();
    let incident = || DeploymentEventKind::IncidentResolved {
        incident: "inc-1".to_owned(),
        detail: "the queue drained".to_owned(),
    };

    let mut unnamed = event("evt-1", 2, 2_000, incident());
    unnamed.id = "   ".to_owned();
    assert!(apply(&model, &unnamed).is_err());

    let mut epochless = event("evt-2", 2, 2_000, incident());
    epochless.epoch = 0;
    assert!(apply(&model, &epochless).is_err());

    let mut unproven = event("evt-3", 2, 2_000, incident());
    unproven.evidence = EvidenceRef::untimed(EvidenceSource::Event, "");
    assert!(apply(&model, &unproven).is_err());

    assert_eq!(model.epoch, 1, "none of the refusals moved the model");
}

#[test]
fn the_model_serialises_without_a_secret_canary() {
    let model = fresh_model();
    let json = model.canonical_json().expect("the model is serialisable");
    for forbidden in [
        "password",
        "secret",
        "token",
        "connection_string",
        "hunter2",
        "BEGIN PRIVATE KEY",
    ] {
        assert!(
            !json.contains(forbidden),
            "the model must not carry {forbidden}"
        );
    }
    assert!(json.contains("\"edition\":\"enterprise\""));
    assert!(json.contains("spot.deployment.model.v1"));
}
