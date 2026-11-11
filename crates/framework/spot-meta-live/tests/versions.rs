//! The snapshot's version and the effective metadata's own version agree (WP-MET-03.T2).
//!
//! `meta().version()` is what the HTTP layer serves as the `ETag` of `GET /api/v1/meta/{doctype}`,
//! so it must move whenever the site's metadata version moves, on the node that applied a change
//! and on a node that adopted it.

use std::sync::Arc;
use std::time::Duration;

use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, FieldType, Property};
use spot_meta_live::{
    Adopt, Follower, InvalidationBus, LiveMeta, MemoryVersionStore, MetaChange, MetaHash,
    MetaLoader, PollSchedule, PollingBus, Version, VersionStore,
};

fn site() -> SiteId {
    SiteId::new("acme.example").unwrap_or_else(|e| panic!("valid site: {e}"))
}

fn widget() -> Definition {
    let mut definition = Definition::new("widget");
    let mut title = FieldSpec::new("title", FieldType::Text);
    title.label = Some("Title".into());
    definition.fields.push(title);
    definition
}

fn widget_titled(label: &str) -> Definition {
    let mut definition = widget();
    definition.fields[0].label = Some(label.into());
    definition
}

fn hash_of(definitions: &[Definition]) -> MetaHash {
    MetaHash::of_definitions(definitions).unwrap_or_else(|e| panic!("hash: {e}"))
}

fn label(text: &str) -> MetaChange {
    MetaChange::SetProperty {
        doctype: "widget".into(),
        field: Some("title".into()),
        property: Property::Label(text.into()),
    }
}

fn custom_field(name: &str) -> MetaChange {
    MetaChange::AddCustomField {
        doctype: "widget".into(),
        spec: Box::new(FieldSpec::new(name, FieldType::Text)),
        insert_after: None,
    }
}

#[test]
fn an_adopted_snapshot_reports_the_version_it_was_adopted_at() {
    let live = LiveMeta::new(site(), vec![widget()]).unwrap_or_else(|e| panic!("builds: {e}"));

    for raw in [5, 9] {
        let offered = widget_titled(&format!("Title {raw}"));
        let hash = hash_of(std::slice::from_ref(&offered));
        let version = Version::new(raw);
        match live.adopt(version, &hash, vec![offered]) {
            Ok(Adopt::Applied(applied)) => assert_eq!(applied, version),
            other => panic!("expected Applied, got {other:?}"),
        }
        let current = live.current();
        assert_eq!(current.version(), version);
        assert_eq!(current.meta().version(), raw, "the metadata agrees");
        assert_eq!(current.plans().meta_version(), raw, "and so do its plans");
    }
}

#[test]
fn a_node_booted_at_a_version_reports_it() {
    let live = LiveMeta::at_version(site(), vec![widget()], Version::new(7))
        .unwrap_or_else(|e| panic!("builds: {e}"));
    assert_eq!(live.current().meta().version(), 7);
    // The next local change continues from there.
    let next = live
        .apply(label("Heading"))
        .unwrap_or_else(|e| panic!("apply: {e}"));
    assert_eq!(next, Version::new(8));
    assert_eq!(live.current().meta().version(), 8);
}

#[test]
fn every_local_change_moves_both_numberings_together() {
    let live = LiveMeta::at_version(site(), vec![widget()], Version::new(3))
        .unwrap_or_else(|e| panic!("builds: {e}"));
    let mut last = live.current().meta().version();
    for change in [label("One"), custom_field("custom_a"), label("Two")] {
        let version = live.apply(change).unwrap_or_else(|e| panic!("apply: {e}"));
        let snapshot = live.current();
        assert_eq!(version, snapshot.version());
        assert_eq!(snapshot.meta().version(), version.get());
        assert!(snapshot.meta().version() > last, "strictly rising");
        last = snapshot.meta().version();
    }
    // A change that fails to build changes neither number.
    let before = live.current();
    let bad = MetaChange::AddCustomField {
        doctype: "widget".into(),
        spec: Box::new(FieldSpec::new("no_prefix", FieldType::Text)),
        insert_after: None,
    };
    assert!(live.apply(bad).is_err());
    assert_eq!(live.current().version(), before.version());
    assert_eq!(live.current().meta().version(), before.meta().version());
}

#[test]
fn a_label_override_changes_the_version_the_etag_comes_from() {
    let live = LiveMeta::at_version(site(), vec![widget()], Version::new(4))
        .unwrap_or_else(|e| panic!("builds: {e}"));
    let etag_before = live.current().meta().version();
    live.apply(label("Heading"))
        .unwrap_or_else(|e| panic!("apply: {e}"));
    let etag_after = live.current().meta().version();
    assert_ne!(
        etag_before, etag_after,
        "a client's If-None-Match no longer matches"
    );
    live.apply(label("Heading 2"))
        .unwrap_or_else(|e| panic!("apply: {e}"));
    assert_ne!(etag_after, live.current().meta().version());
}

fn node(
    store: &Arc<MemoryVersionStore>,
    bus: &Arc<dyn InvalidationBus>,
    version: Version,
) -> Follower {
    Follower::with_initial(
        site(),
        vec![widget()],
        version,
        Arc::clone(bus),
        Arc::clone(store) as Arc<dyn VersionStore>,
        Arc::clone(store) as Arc<dyn MetaLoader>,
        PollSchedule::new(Duration::from_secs(30)),
    )
    .unwrap_or_else(|e| panic!("the node builds: {e}"))
}

async fn two_nodes_agree_after(start: Version, changes: Vec<MetaChange>) {
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let applier = node(&store, &bus, start);
    let follower = node(&store, &bus, start);

    for change in changes {
        let version = applier
            .apply(change)
            .await
            .unwrap_or_else(|e| panic!("apply: {e}"));
        store.put_definitions(version, applier.current().meta().definitions().to_vec());
        let adopted = follower
            .poll_once()
            .await
            .unwrap_or_else(|e| panic!("poll: {e}"));
        assert_eq!(adopted, Some(version));

        let (a, b) = (applier.current(), follower.current());
        assert_eq!(a.version(), b.version());
        assert_eq!(a.meta().version(), b.meta().version(), "the same ETag");
        assert_eq!(a.meta().version(), version.get());
        assert_eq!(a.hash(), b.hash());
    }
}

#[tokio::test]
async fn a_node_that_applies_and_one_that_adopts_report_the_same_version() {
    two_nodes_agree_after(
        Version::new(4),
        vec![label("One"), custom_field("custom_a"), label("Two")],
    )
    .await;
}

#[tokio::test]
async fn a_chain_that_starts_from_the_empty_site_agrees_from_its_second_change() {
    // `spot-meta` cannot build version 0, so the first change from `Version::ZERO` is version 2
    // (the starting snapshot's metadata already reports 1); every later change is one more.
    let store = Arc::new(MemoryVersionStore::new());
    let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.clone()));
    let applier = node(&store, &bus, Version::ZERO);
    let follower = node(&store, &bus, Version::ZERO);

    let first = applier
        .apply(label("One"))
        .await
        .unwrap_or_else(|e| panic!("apply: {e}"));
    assert_eq!(first, Version::new(2), "the first change is version 2");

    let second = applier
        .apply(label("Two"))
        .await
        .unwrap_or_else(|e| panic!("apply: {e}"));
    assert!(second > first);
    store.put_definitions(second, applier.current().meta().definitions().to_vec());
    assert_eq!(
        follower
            .poll_once()
            .await
            .unwrap_or_else(|e| panic!("poll: {e}")),
        Some(second)
    );
    let (a, b) = (applier.current(), follower.current());
    assert_eq!(a.meta().version(), second.get());
    assert_eq!(a.meta().version(), b.meta().version());
    assert_eq!(a.hash(), b.hash());
}

#[test]
fn a_hash_divergence_still_refuses_and_keeps_the_snapshot() {
    let live = LiveMeta::at_version(site(), vec![widget()], Version::new(2))
        .unwrap_or_else(|e| panic!("builds: {e}"));
    let before = live.current();

    // The same version with other content is divergence; the snapshot and its numbers stay.
    let other = hash_of(&[widget_titled("Other")]);
    match live.adopt(before.version(), &other, vec![widget_titled("Other")]) {
        Err(error) => assert!(error.is_divergence(), "got {error:?}"),
        other => panic!("expected divergence, got {other:?}"),
    }
    // A newer version whose definitions do not hash to the announced hash is divergence too.
    match live.adopt(Version::new(6), &other, vec![widget_titled("Different")]) {
        Err(error) => assert!(error.is_divergence(), "got {error:?}"),
        other => panic!("expected divergence, got {other:?}"),
    }
    let after = live.current();
    assert!(Arc::ptr_eq(&before, &after));
    assert_eq!(after.meta().version(), 2);
}

#[test]
fn a_site_with_permlevel_fields_can_be_adopted() {
    // The publisher's effective definitions carry the generated `level_N` group; a follower
    // rebuilds them (WP-MET-01.T9) at the announced version and gets the same hash.
    let mut definition = Definition::new("invoice");
    let mut amount = FieldSpec::new("amount", FieldType::Decimal);
    amount.scale = Some(2);
    amount.permlevel = 2;
    definition.fields.push(amount);

    let publisher = LiveMeta::at_version(site(), vec![definition], Version::new(3))
        .unwrap_or_else(|e| panic!("the publisher builds: {e}"));
    let published = publisher.current();
    let effective = published.meta().definitions().to_vec();

    let follower = LiveMeta::new(site(), Vec::new()).unwrap_or_else(|e| panic!("builds: {e}"));
    match follower.adopt(Version::new(5), &hash_of(&effective), effective) {
        Ok(Adopt::Applied(applied)) => assert_eq!(applied, Version::new(5)),
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(follower.current().meta().version(), 5);
    assert_eq!(follower.current().hash(), published.hash());
}
