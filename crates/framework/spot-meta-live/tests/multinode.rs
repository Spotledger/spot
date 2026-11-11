//! The multi-node story (WP-MET-03 acceptance): a change on node A is visible on node B within
//! one poll, a restarted node catches up, a lost message is repaired, and a version/hash clash
//! is divergence (WP-MET-03.T1, deliverables 2–5).

use std::sync::Arc;
use std::time::Duration;

use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_meta_live::{
    Follower, Invalidation, InvalidationBus, LiveMeta, MemoryVersionStore, MetaChange, MetaLoader,
    PollSchedule, PollingBus, Version, VersionStore,
};

fn site() -> SiteId {
    SiteId::new("acme.example").unwrap_or_else(|e| panic!("valid site: {e}"))
}

/// A DocType with one text field `field`.
fn widget_with(field: &str) -> Definition {
    let mut definition = Definition::new("widget");
    definition
        .fields
        .push(FieldSpec::new(field, FieldType::Text));
    definition
}

fn definitions() -> Vec<Definition> {
    vec![widget_with("title")]
}

/// A posted DocType: `spot-meta` adds `doc_state` and `amended_from` to it at build time, which
/// the reload path must not duplicate.
fn posted_widget() -> Definition {
    let mut definition = widget_with("title");
    definition.is_posted = true;
    definition
}

/// A node that boots at `version` with `initial`, subscribing to `bus` and polling `store`.
fn node(
    site: SiteId,
    store: &Arc<MemoryVersionStore>,
    bus: &Arc<dyn InvalidationBus>,
    initial: Vec<Definition>,
    version: Version,
) -> Follower {
    Follower::with_initial(
        site,
        initial,
        version,
        Arc::clone(bus),
        Arc::clone(store) as Arc<dyn VersionStore>,
        Arc::clone(store) as Arc<dyn MetaLoader>,
        PollSchedule::new(Duration::from_secs(30)),
    )
    .unwrap_or_else(|e| panic!("the node builds: {e}"))
}

/// Applies `change` on `follower` and mirrors the new effective definitions into `store`, as
/// the metadata-persistence step (WP-MET-01.T2) does in a real deployment.
async fn apply_and_persist(
    store: &MemoryVersionStore,
    follower: &Follower,
    change: MetaChange,
) -> Version {
    let version = follower
        .apply(change)
        .await
        .unwrap_or_else(|e| panic!("apply: {e}"));
    let snapshot = follower.current();
    store.put_definitions(version, snapshot.meta().definitions().to_vec());
    version
}

fn custom_field(name: &str) -> MetaChange {
    MetaChange::AddCustomField {
        doctype: "widget".into(),
        spec: Box::new(FieldSpec::new(name, FieldType::Text)),
        insert_after: None,
    }
}

#[tokio::test]
async fn a_change_on_node_a_is_visible_on_node_b_within_one_poll() {
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let site = site();

    let a = node(site.clone(), &store, &bus, definitions(), Version::ZERO);
    let b = node(site.clone(), &store, &bus, definitions(), Version::ZERO);

    let version = apply_and_persist(&store, &a, custom_field("custom_colour")).await;

    assert_eq!(
        b.current().version(),
        Version::ZERO,
        "B has not polled, so it still serves its own snapshot"
    );

    let adopted = b.poll_once().await.unwrap_or_else(|e| panic!("poll: {e}"));
    assert_eq!(adopted, Some(version), "one poll adopts A's version");

    let snapshot = b.current();
    assert_eq!(snapshot.version(), version);
    assert!(
        snapshot
            .meta()
            .definition("widget")
            .is_some_and(|d| d.fields.iter().any(|f| f.name == "custom_colour")),
        "A's custom field is in B's metadata"
    );
    assert!(
        snapshot
            .plan("widget")
            .is_some_and(|plan| plan.field_ix("custom_colour").is_some()),
        "and B validated it into its plan"
    );
}

#[tokio::test]
async fn a_posted_doctype_rebuilds_on_the_follower() {
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let site = site();

    let a = node(
        site.clone(),
        &store,
        &bus,
        vec![posted_widget()],
        Version::ZERO,
    );
    let b = node(
        site.clone(),
        &store,
        &bus,
        vec![posted_widget()],
        Version::ZERO,
    );

    let version = apply_and_persist(&store, &a, custom_field("custom_colour")).await;
    let adopted = b.poll_once().await.unwrap_or_else(|e| panic!("poll: {e}"));
    assert_eq!(adopted, Some(version));

    let snapshot = b.current();
    let definition = snapshot
        .meta()
        .definition("widget")
        .unwrap_or_else(|| panic!("widget is present"));
    let engine_fields = definition
        .fields
        .iter()
        .filter(|field| field.name == spot_meta::DOC_STATE)
        .count();
    assert_eq!(
        engine_fields, 1,
        "exactly one doc_state field survives the reload"
    );
    assert!(
        snapshot
            .plan("widget")
            .is_some_and(|plan| plan.field_ix("custom_colour").is_some()),
        "the custom field is planned on the follower too"
    );
}

#[tokio::test]
async fn a_restarted_node_catches_up_from_the_store() {
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let site = site();

    let a = node(site.clone(), &store, &bus, definitions(), Version::ZERO);
    let first = apply_and_persist(&store, &a, custom_field("custom_colour")).await;
    let second = apply_and_persist(&store, &a, custom_field("custom_size")).await;
    assert!(second > first, "versions rise strictly");

    // A fresh process starting at version zero reconciles to the head in one poll.
    let restarted = node(site.clone(), &store, &bus, definitions(), Version::ZERO);
    let adopted = restarted
        .poll_once()
        .await
        .unwrap_or_else(|e| panic!("poll: {e}"));
    assert_eq!(adopted, Some(second));
    assert_eq!(restarted.current().version(), second);
    assert!(
        restarted
            .current()
            .plan("widget")
            .is_some_and(|plan| plan.field_ix("custom_size").is_some()),
        "the restarted node serves the latest plan"
    );
}

#[tokio::test]
async fn a_lost_message_is_repaired_by_the_next_poll() {
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let site = site();

    let b = node(site.clone(), &store, &bus, definitions(), Version::ZERO);

    // Node A changed the store, but B missed the bus message entirely.
    let a = LiveMeta::new(site.clone(), definitions()).unwrap_or_else(|e| panic!("A: {e}"));
    a.apply(custom_field("custom_colour"))
        .unwrap_or_else(|e| panic!("apply: {e}"));
    let snapshot = a.current();
    store.publish(
        snapshot.version(),
        snapshot.hash().clone(),
        snapshot.meta().definitions().to_vec(),
    );

    let repaired = b.poll_once().await.unwrap_or_else(|e| panic!("poll: {e}"));
    assert_eq!(repaired, Some(snapshot.version()));
    assert_eq!(b.current().version(), snapshot.version());
}

#[tokio::test]
async fn a_bus_gap_reconciles_from_the_store() {
    let store = Arc::new(MemoryVersionStore::new());
    // Capacity one, so the second change overflows B's local queue.
    let bus = Arc::new(PollingBus::with_capacity(store.clone(), 1));
    let bus_dyn: Arc<dyn InvalidationBus> = Arc::clone(&bus) as Arc<dyn InvalidationBus>;
    let site = site();

    let mut b = node(site.clone(), &store, &bus_dyn, definitions(), Version::ZERO);

    let applier = LiveMeta::new(site.clone(), definitions()).unwrap_or_else(|e| panic!("A: {e}"));
    for name in ["custom_colour", "custom_size"] {
        applier
            .apply(custom_field(name))
            .unwrap_or_else(|e| panic!("apply {name}: {e}"));
        let snapshot = applier.current();
        store.publish(
            snapshot.version(),
            snapshot.hash().clone(),
            snapshot.meta().definitions().to_vec(),
        );
        bus.publish(
            &site,
            Invalidation::meta(snapshot.version(), snapshot.hash().clone()),
        )
        .await
        .unwrap_or_else(|e| panic!("publish {name}: {e}"));
    }
    let head = store
        .head()
        .unwrap_or_else(|| panic!("the store has a head"));

    // The overflowed queue reports a gap; the follower reconciles to the head from the store.
    let adopted = b
        .deliver_next()
        .await
        .unwrap_or_else(|e| panic!("deliver: {e}"));
    assert_eq!(
        adopted,
        Some(head.0),
        "the gap reconciled to the stored head"
    );
    assert_eq!(b.current().version(), head.0);
}

#[tokio::test]
async fn the_same_version_with_a_different_hash_is_divergence() {
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let site = site();

    // Two different contents, both at version one.
    let one = LiveMeta::at_version(site.clone(), vec![widget_with("title")], Version::new(1))
        .unwrap_or_else(|e| panic!("A: {e}"));
    let b = node(
        site.clone(),
        &store,
        &bus,
        vec![widget_with("name")],
        Version::new(1),
    );
    let snapshot = one.current();
    store.publish(
        Version::new(1),
        snapshot.hash().clone(),
        snapshot.meta().definitions().to_vec(),
    );

    match b.poll_once().await {
        Err(error) => {
            assert!(error.is_divergence(), "expected divergence, got {error:?}");
            assert_eq!(error.code(), "metalive.divergence");
        }
        Ok(adopted) => panic!("expected divergence, got {adopted:?}"),
    }
    assert_eq!(
        b.current().version(),
        Version::new(1),
        "B keeps serving its own snapshot"
    );
}
