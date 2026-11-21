//! Subscription behaviour: convergence, buffered gaps, self-check refresh and epoch stamps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::{app, base_facts, event, migration};
use spot_audit_chain::{AuditRecord, Payload, PayloadValue};
use spot_deployment_model::{
    ConfigChange, DeploymentEvent, DeploymentEventKind, Digest, HandleOutcome, MemoryFacts,
    ModelHandle, NodeRole, Replica, ReplicaRole, apply, refresh, verify,
};
use spot_deployment_model::{DIGEST_PAYLOAD_KEY, EPOCH_PAYLOAD_KEY};

fn initial_model() -> spot_deployment_model::DeploymentModel {
    let source = MemoryFacts::with_facts(1, base_facts());
    refresh(None, &[&source], 1_000).expect("the fresh model builds")
}

#[test]
fn a_refresh_names_the_record_that_proved_it() {
    let evidence = spot_deployment_model::EvidenceRef::at(
        spot_deployment_model::EvidenceSource::Installer,
        "installer:generation:7",
        900,
    );
    let source = MemoryFacts::with_facts(1, base_facts()).with_evidence(evidence.clone());
    let model = refresh(None, &[&source], 1_000).expect("the fresh model builds");
    let entry = model.ledger.last().expect("the refresh is in the ledger");
    assert_eq!(entry.event, "refresh");
    assert_eq!(entry.evidence, Some(evidence));
}

/// Five events in epoch order: a migration, an install, an upgrade, a node joining and a
/// replica promotion.
fn chain() -> Vec<DeploymentEvent> {
    vec![
        event(
            "evt-migration",
            2,
            2_000,
            DeploymentEventKind::MigrationApplied {
                run: migration("mig-1", 2_000).run,
                outcome: "applied".to_owned(),
            },
        ),
        event(
            "evt-install",
            3,
            3_000,
            DeploymentEventKind::AppInstalled {
                app: "erp_selling".to_owned(),
                version: "1.0.0".to_owned(),
            },
        ),
        event(
            "evt-upgrade",
            4,
            4_000,
            DeploymentEventKind::AppUpgraded {
                app: "erp_selling".to_owned(),
                from: "1.0.0".to_owned(),
                to: "1.1.0".to_owned(),
            },
        ),
        event(
            "evt-node",
            5,
            5_000,
            DeploymentEventKind::NodeJoined {
                node: "ln-2".to_owned(),
                roles: [NodeRole::Web].into_iter().collect(),
            },
        ),
        event(
            "evt-promote",
            6,
            6_000,
            DeploymentEventKind::ConfigChanged {
                change: ConfigChange::Backend(
                    spot_deployment_model::Backend::new(
                        spot_deployment_model::BackendEngine::Postgres,
                        "16.4",
                        vec![
                            Replica::new("ln-1", ReplicaRole::Standby, true).unwrap(),
                            Replica::new("ln-2", ReplicaRole::Primary, false).unwrap(),
                        ],
                    )
                    .unwrap(),
                ),
            },
        ),
    ]
}

fn in_order() -> spot_deployment_model::DeploymentModel {
    let mut model = initial_model();
    for event in chain() {
        model = match apply(&model, &event).expect("the event applies") {
            spot_deployment_model::ApplyOutcome::Applied { model, .. } => *model,
            other => panic!("expected an applied event, got {other:?}"),
        };
    }
    model
}

#[test]
fn shuffled_and_duplicated_events_converge_on_the_same_model() {
    let expected = in_order();
    let handle = ModelHandle::new(initial_model());

    // Out of order: the sixth event, the fourth, then the rest in order. Each gap is
    // buffered, and a buffered event applies as soon as its predecessors arrive.
    for index in [4_usize, 2, 0, 1, 3] {
        let outcome = handle.apply(chain()[index].clone()).expect("no refusal");
        if index == 0 {
            assert_eq!(outcome, HandleOutcome::Applied { epoch: 2 });
        }
    }
    assert_eq!(handle.epoch(), 6);
    assert_eq!(
        *handle.current(),
        expected,
        "event order does not change the model"
    );
    assert_eq!(
        handle.current().digest().unwrap(),
        expected.digest().unwrap()
    );

    // Every event delivered a second time is a duplicate, never a second application.
    for event in chain() {
        assert!(matches!(
            handle.apply(event).expect("a duplicate is not an error"),
            HandleOutcome::Duplicate { .. }
        ));
    }
    assert_eq!(*handle.current(), expected);
}

#[test]
fn a_gap_is_buffered_until_the_refresh_or_the_missing_event_arrives() {
    let handle = ModelHandle::new(initial_model());
    let events = chain();

    let queued = handle
        .apply(events[4].clone())
        .expect("the future event queues");
    assert_eq!(
        queued,
        HandleOutcome::Queued {
            epoch: 6,
            expected: 2,
        }
    );
    assert!(queued.needs_refresh());
    assert_eq!(
        handle.epoch(),
        1,
        "the model did not guess the missing epochs"
    );

    // The predecessor chain arrives: the buffered event applies with it.
    for event in &events[..4] {
        handle.apply(event.clone()).expect("the event applies");
    }
    assert_eq!(handle.epoch(), 6);
    assert!(handle.current().node("ln-2").is_some());
}

#[test]
fn drift_is_detected_and_repaired_by_the_self_check() {
    let handle = ModelHandle::new(initial_model());

    // Reality moved on while the agent was not listening: an app was installed.
    let mut drifted = base_facts();
    drifted
        .apps
        .get_or_insert_with(BTreeMap::new)
        .insert("erp_selling".to_owned(), app("2.0.0"));
    let source = MemoryFacts::with_facts(7, drifted.clone());

    let mismatches = handle.verify(&[&source]).expect("the sources are readable");
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].field, "apps");

    let check = handle
        .self_check(&[&source], 10_000)
        .expect("the self-check runs");
    assert!(!check.is_clean());
    assert_eq!(check.refreshed_to, Some(2));
    assert_eq!(handle.epoch(), 2);
    assert_eq!(
        handle
            .current()
            .app("erp_selling")
            .map(|app| app.version.as_str()),
        Some("2.0.0")
    );
    assert!(verify(&handle.current(), &drifted).is_empty());

    // The next check is clean and does not move the epoch.
    let clean = handle
        .self_check(&[&source], 11_000)
        .expect("the second check runs");
    assert!(clean.is_clean());
    assert_eq!(clean.refreshed_to, None);
    assert_eq!(handle.epoch(), 2);
}

#[test]
fn clones_share_one_channel_and_one_pending_buffer() {
    let handle = ModelHandle::new(initial_model());
    let clone = handle.clone();
    let events = chain();

    // The clone queues the future event; the original sees it pending and then applies it.
    clone
        .apply(events[4].clone())
        .expect("the future event queues");
    for event in &events[..4] {
        handle.apply(event.clone()).expect("the event applies");
    }
    assert_eq!(handle.epoch(), 6);
    assert_eq!(clone.epoch(), 6);
    assert_eq!(*clone.current(), *handle.current());
}

#[tokio::test]
async fn a_subscriber_wakes_on_every_new_epoch() {
    let handle = ModelHandle::new(initial_model());
    let mut receiver = handle.subscribe();
    receiver.borrow_and_update();
    assert_eq!(receiver.borrow().epoch, 1);

    handle.apply(chain()[0].clone()).expect("the event applies");
    receiver.changed().await.expect("the channel is open");
    assert_eq!(receiver.borrow().epoch, 2);

    // A clone publishes too: every subscriber sees one channel.
    handle
        .clone()
        .apply(chain()[1].clone())
        .expect("the event applies");
    receiver.changed().await.expect("the channel is open");
    assert_eq!(receiver.borrow().epoch, 3);
    assert_eq!(
        receiver
            .borrow()
            .app("erp_selling")
            .map(|app| app.version.as_str()),
        Some("1.0.0")
    );
}

#[test]
fn an_action_carries_the_epoch_and_digest_it_relied_on() {
    let handle = ModelHandle::new(initial_model());
    handle.apply(chain()[0].clone()).expect("the event applies");

    let stamp = handle.stamp().expect("the model stamps");
    assert_eq!(stamp.epoch, 2);
    assert_eq!(stamp.digest, handle.current().digest().unwrap());
    assert_ne!(stamp.digest, Digest::of(b"not the model"));

    let mut record = AuditRecord {
        kind: "agent.decision".to_owned(),
        actor: "agent:glm".to_owned(),
        on_behalf_of: None,
        subject: None,
        at: 9_000,
        payload: Payload::new(),
        masked: true,
    };
    stamp.attach(&mut record);
    assert_eq!(
        record.payload.get(EPOCH_PAYLOAD_KEY),
        Some(&PayloadValue::Int(2))
    );
    assert_eq!(
        record.payload.get(DIGEST_PAYLOAD_KEY),
        Some(&PayloadValue::Text(stamp.digest.to_string()))
    );
    assert!(
        record.validate().is_ok(),
        "the stamp is a valid audit payload"
    );
}

#[test]
fn the_same_facts_at_two_epochs_share_a_digest() {
    let model = initial_model();
    let later = spot_deployment_model::DeploymentModel::from_facts(
        model.epoch + 41,
        &model.facts(),
        model.sources.clone(),
    )
    .expect("the facts rebuild");
    assert_eq!(later.digest().unwrap(), model.digest().unwrap());
    assert_ne!(later.epoch, model.epoch);
}

#[test]
fn a_handle_with_no_event_keeps_the_initial_epoch() {
    let handle = ModelHandle::new(initial_model());
    assert_eq!(handle.epoch(), 1);
    assert_eq!(handle.current().apps.len(), 0);
    assert!(handle.current().overrides.is_empty());
}
