#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! WP-MET-03.T3: the version the engine sees (`meta().version()`) rises on every runtime
//! customisation, on the node that applied it and on a follower that adopted it, and equals the
//! snapshot's own site version. A cache keyed on it must never serve a stale plan or permission
//! set.

mod common;
use common::*;
use spot_customise::Node;
use spot_meta::{FieldSpec, FieldType};

fn field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

#[tokio::test]
async fn the_metadata_version_rises_on_every_step_on_the_applying_node_and_on_a_follower() {
    let rig = Rig::new().await;
    let node = Node::new(rig.follower());
    let mut before_live = rig.live.current().meta().version();
    let mut before_follower = node.snapshot().unwrap().meta().version();

    // A DocType, a field, a property, a removal: each is a new, strictly higher number.
    for step in 0..4 {
        match step {
            0 => {
                rig.service
                    .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
                    .await
                    .unwrap();
            }
            1 => {
                rig.service
                    .add_custom_field(&admin(), "invoice", &field("custom_a"), None)
                    .await
                    .unwrap();
            }
            2 => {
                rig.service
                    .set_property(&admin(), "invoice", Some("amount"), "read_only", "true")
                    .await
                    .unwrap();
            }
            _ => {
                rig.service
                    .remove_custom_field(&admin(), "invoice", "custom_a")
                    .await
                    .unwrap();
            }
        }
        let current = rig.live.current();
        assert!(
            current.meta().version() > before_live,
            "step {step}: the applying node's metadata version rose ({} -> {})",
            before_live,
            current.meta().version()
        );
        assert_eq!(
            current.meta().version(),
            current.version().get(),
            "step {step}: the metadata version is the site version"
        );
        before_live = current.meta().version();

        node.poll().await.unwrap();
        let adopted = node.snapshot().unwrap();
        assert!(
            adopted.meta().version() > before_follower,
            "step {step}: the follower's metadata version rose ({} -> {})",
            before_follower,
            adopted.meta().version()
        );
        assert_eq!(adopted.meta().version(), adopted.version().get());
        assert_eq!(adopted.meta().version(), current.meta().version());
        before_follower = adopted.meta().version();
    }
}

/// What a cache keyed on the served metadata version sees across one change.
async fn first_change_is_seen_by_the_cache_and_the_follower(
    rig: &Rig,
    change: impl std::future::Future<Output = ()>,
) {
    let node = Node::new(rig.follower());
    let before = rig.live.current();
    // A fresh site starts at `Version::ZERO`, whose metadata already reports version 1.
    assert_eq!(before.version(), spot_meta_live::Version::ZERO);
    assert_eq!(before.meta().version(), 1);
    let mut cache: std::collections::HashMap<u64, &'static str> = std::collections::HashMap::new();
    cache.insert(before.meta().version(), "plan before the change");
    let etag_input_before = before.meta().version();

    change.await;

    let after = rig.live.current();
    assert_eq!(
        after.meta().version(),
        2,
        "the first change is version 2, never 1"
    );
    assert_eq!(after.version().get(), 2);
    assert!(
        !cache.contains_key(&after.meta().version()),
        "the cache keyed on the metadata version must miss after the first change"
    );
    assert_ne!(
        etag_input_before,
        after.meta().version(),
        "the metadata ETag input changed"
    );

    node.poll().await.unwrap();
    let adopted = node.snapshot().unwrap();
    assert_eq!(adopted.meta().version(), after.meta().version());
    assert_eq!(adopted.version(), after.version());
}

#[tokio::test]
async fn the_first_customisation_on_a_fresh_site_changes_the_metadata_version() {
    let rig = Rig::new().await;
    first_change_is_seen_by_the_cache_and_the_follower(&rig, async {
        rig.service
            .add_custom_field(&admin(), "invoice", &field("custom_first"), None)
            .await
            .unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_first_doctype_on_a_fresh_site_changes_the_metadata_version() {
    let rig = Rig::new().await;
    first_change_is_seen_by_the_cache_and_the_follower(&rig, async {
        rig.service
            .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
            .await
            .unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_first_property_on_a_fresh_site_changes_the_metadata_version() {
    let rig = Rig::new().await;
    first_change_is_seen_by_the_cache_and_the_follower(&rig, async {
        rig.service
            .set_property(&admin(), "invoice", Some("amount"), "read_only", "true")
            .await
            .unwrap();
    })
    .await;
}
