#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! Cross-node behaviour: a second node converges within one poll, a service on a second node
//! catches up from the shared records, and a node that cannot reproduce what was announced
//! refuses to serve metadata rather than guess.

mod common;
use common::*;
use spot_customise::Node;
use spot_meta::{FieldSpec, FieldType};
use spot_meta_live::{Follower, InvalidationBus, MetaLoader, PollSchedule, Version, VersionStore};
use std::sync::Arc;
use std::time::Duration;

fn field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

#[tokio::test]
async fn a_second_node_serves_the_new_type_within_one_poll() {
    let rig = Rig::new().await;
    let node = Node::new(rig.follower());
    assert!(
        node.snapshot()
            .unwrap()
            .meta()
            .definition("ticket")
            .is_none()
    );

    let applied = rig
        .service
        .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
        .await
        .unwrap();
    assert!(
        node.snapshot()
            .unwrap()
            .meta()
            .definition("ticket")
            .is_none(),
        "B has not polled yet"
    );
    let adopted = node.poll().await.unwrap();
    assert_eq!(adopted, Some(applied.version));
    let snapshot = node.snapshot().unwrap();
    assert!(snapshot.meta().definition("ticket").is_some());
    assert!(snapshot.plan("ticket").is_some(), "B compiled its plan");
    assert_eq!(snapshot.hash(), rig.live.current().hash(), "same content");

    // A property override and a removal converge too.
    rig.service
        .set_property(&admin(), "invoice", Some("amount"), "read_only", "true")
        .await
        .unwrap();
    rig.service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap();
    rig.service
        .remove_custom_field(&admin(), "invoice", "custom_a")
        .await
        .unwrap();
    node.poll().await.unwrap();
    let snapshot = node.snapshot().unwrap();
    assert_eq!(snapshot.hash(), rig.live.current().hash());
    assert!(
        snapshot
            .meta()
            .definition("invoice")
            .unwrap()
            .fields
            .iter()
            .find(|f| f.name == "amount")
            .unwrap()
            .read_only
    );
    // Nothing new: the next poll adopts nothing.
    assert_eq!(node.poll().await.unwrap(), None);
}

#[tokio::test]
async fn a_service_on_a_second_node_catches_up_from_the_shared_records() {
    let durable = Durable::new().await;
    let a = Rig::boot(&durable).await;
    a.service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap();
    let head = durable.versions.head().unwrap();

    let b = Rig::boot(&durable).await;
    assert!(
        b.live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .fields
            .iter()
            .all(|f| f.name != "custom_a")
    );
    let recovery = b.service.recover().await.unwrap();
    assert_eq!(
        recovery.swapped,
        Some(head.0),
        "B adopts the announced version"
    );
    assert_eq!(
        durable.versions.head().unwrap(),
        head,
        "and announces nothing new"
    );
    assert_eq!(b.live.current().hash(), a.live.current().hash());

    // B changes something; A catches up on its next call (or recover) and agrees.
    b.service
        .set_property(&admin(), "invoice", None, "label", "Bill")
        .await
        .unwrap();
    a.service.recover().await.unwrap();
    assert_eq!(a.live.current().hash(), b.live.current().hash());
    assert_eq!(a.live.current().version(), b.live.current().version());
    assert_eq!(
        a.live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .label
            .as_deref(),
        Some("Bill")
    );
}

#[tokio::test]
async fn a_node_that_cannot_reproduce_the_announced_metadata_refuses_to_serve_it() {
    let rig = Rig::new().await;
    rig.service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap();
    let announced = rig.versions.head().unwrap();

    // Node B holds the same version number with different content.
    let mut other = base_definitions();
    other[0].label = Some("Counterparty".into());
    let follower = Follower::with_initial(
        site(),
        other,
        announced.0,
        rig.bus.clone() as Arc<dyn InvalidationBus>,
        rig.versions.clone() as Arc<dyn VersionStore>,
        rig.versions.clone() as Arc<dyn MetaLoader>,
        PollSchedule::new(Duration::from_secs(30)),
    )
    .unwrap();
    let node = Node::new(follower);
    assert!(
        node.snapshot().is_ok(),
        "serving until it learns of the clash"
    );

    let err = node.poll().await.unwrap_err();
    assert_eq!(err.code(), "customise.divergence");
    assert!(err.to_string().contains("metalive.divergence"), "{err}");
    assert!(node.divergence().is_some(), "reported");
    let refused = node.snapshot().unwrap_err();
    assert_eq!(
        refused.code(),
        "customise.divergence",
        "no metadata is served"
    );

    // A later version the node can build clears it.
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Bill")
        .await
        .unwrap();
    assert!(node.poll().await.unwrap().is_some());
    assert!(node.divergence().is_none());
    assert_eq!(node.snapshot().unwrap().hash(), rig.live.current().hash());
    assert!(Version::ZERO < rig.live.current().version());
}

#[tokio::test]
async fn a_foreign_head_is_never_overwritten() {
    let rig = Rig::new().await;
    rig.service
        .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
        .await
        .unwrap();
    // Someone else publishes a different head above the version the service holds.
    let head = rig.versions.head().unwrap();
    rig.versions.publish(
        Version::new(head.0.get() + 1),
        spot_meta_live::MetaHash::from_hex("0".repeat(64)),
        base_definitions(),
    );
    // The service's next call sees a head it cannot match from its records: it publishes above
    // the foreign head rather than overwrite it.
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Bill")
        .await
        .unwrap();
    let after = rig.versions.head().unwrap();
    assert!(
        after.0.get() > head.0.get() + 1,
        "published above the foreign version"
    );
    assert_eq!(&after.1, rig.live.current().hash());
}
