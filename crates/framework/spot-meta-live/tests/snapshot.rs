//! The snapshot holder: a failed build changes nothing, and readers racing a swap never see a
//! half-built set (WP-MET-03.T1, deliverable 1).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, FieldType, Property};
use spot_meta_live::{LiveError, LiveMeta, MetaChange, MetaHash};

fn site() -> SiteId {
    SiteId::new("acme.example").unwrap_or_else(|e| panic!("valid site: {e}"))
}

/// A DocType with one text field.
fn widget() -> Definition {
    let mut definition = Definition::new("widget");
    let mut title = FieldSpec::new("title", FieldType::Text);
    title.label = Some("Title".into());
    definition.fields.push(title);
    definition
}

/// A DocType whose one field has `field` as its name, so two builds differ in content.
fn widget_with(field: &str) -> Definition {
    let mut definition = Definition::new("widget");
    definition
        .fields
        .push(FieldSpec::new(field, FieldType::Text));
    definition
}

#[test]
fn a_failed_build_keeps_the_old_snapshot_serving() {
    let live = LiveMeta::new(site(), vec![widget()]).unwrap_or_else(|e| panic!("builds: {e}"));
    let before = live.current();

    // A custom field name without the `custom_` prefix is refused by `spot-meta`'s validation.
    let error = match live.apply(MetaChange::AddCustomField {
        doctype: "widget".into(),
        spec: Box::new(FieldSpec::new("colour", FieldType::Text)),
        insert_after: None,
    }) {
        Ok(version) => panic!("the bad custom field must not build, got version {version}"),
        Err(error) => error,
    };
    match &error {
        LiveError::Build { meta, plans, .. } => {
            assert!(!meta.is_empty(), "the metadata error is reported");
            assert!(plans.is_empty(), "no plan build ran");
        }
        other => panic!("expected a build error, got {other:?}"),
    }
    assert_eq!(error.code(), "metalive.build");

    let after = live.current();
    assert_eq!(after.version(), before.version());
    assert_eq!(after.hash(), before.hash());
    assert!(
        Arc::ptr_eq(&before, &after),
        "the very same snapshot keeps serving"
    );
}

#[test]
fn a_build_reports_every_metadata_error_at_once() {
    // Two DocTypes, each linking to a missing target: both problems must come back together.
    let mut first = Definition::new("widget");
    let mut link = FieldSpec::new("owner", FieldType::Link);
    link.options = Some("missing_one".into());
    first.fields.push(link);

    let mut second = Definition::new("gadget");
    let mut link = FieldSpec::new("owner", FieldType::Link);
    link.options = Some("missing_two".into());
    second.fields.push(link);

    let live = LiveMeta::new(site(), vec![first, second]);
    match live {
        Ok(_) => panic!("definitions with bad links must not build"),
        Err(LiveError::Build { meta, .. }) => {
            assert!(
                meta.len() >= 2,
                "both unknown-link errors are reported, got {}",
                meta.len()
            );
        }
        Err(other) => panic!("expected a build error, got {other:?}"),
    }
}

#[test]
fn sixty_four_readers_racing_a_swap_never_see_a_mixture() {
    let live =
        Arc::new(LiveMeta::new(site(), vec![widget()]).unwrap_or_else(|e| panic!("builds: {e}")));
    let stop = Arc::new(AtomicBool::new(false));
    let max_version = Arc::new(AtomicU64::new(0));

    let mut readers = Vec::new();
    for _ in 0..64 {
        let live = Arc::clone(&live);
        let stop = Arc::clone(&stop);
        let max_version = Arc::clone(&max_version);
        readers.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let snapshot = live.current();
                // A snapshot carries exactly one build: its stored hash is the hash of the
                // metadata it holds, and its plans came from the same metadata version.
                let recomputed = MetaHash::of_definitions(snapshot.meta().definitions())
                    .unwrap_or_else(|e| panic!("rehash: {e}"));
                assert_eq!(
                    snapshot.hash(),
                    &recomputed,
                    "a reader saw a hash from one build and metadata from another"
                );
                assert_eq!(
                    snapshot.plans().meta_version(),
                    snapshot.meta().version(),
                    "a reader saw plans from one build and metadata from another"
                );
                let _ = max_version.fetch_max(snapshot.version().get(), Ordering::Relaxed);
            }
        }));
    }

    // Apply additive changes while the readers race the pointer.
    let mut applied = 0u64;
    for i in 0..8 {
        applied += 1;
        live.apply(MetaChange::AddCustomField {
            doctype: "widget".into(),
            spec: Box::new(FieldSpec::new(
                &format!("custom_field_{i}"),
                FieldType::Text,
            )),
            insert_after: None,
        })
        .unwrap_or_else(|e| panic!("add custom field {i}: {e}"));
    }

    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader
            .join()
            .unwrap_or_else(|_| panic!("a reader panicked"));
    }
    assert_eq!(applied, 8);
    assert!(
        max_version.load(Ordering::Relaxed) > 0,
        "at least one reader saw a changed snapshot"
    );
}

#[test]
fn a_held_snapshot_is_not_mutated_by_a_later_change() {
    let live = LiveMeta::new(site(), vec![widget()]).unwrap_or_else(|e| panic!("builds: {e}"));
    let held = live.current();
    let held_version = held.version();
    let held_hash = held.hash().clone();

    live.apply(MetaChange::SetProperty {
        doctype: "widget".into(),
        field: None,
        property: Property::Label("Widget".into()),
    })
    .unwrap_or_else(|e| panic!("label override: {e}"));

    // A request that started on version N keeps seeing exactly that snapshot.
    assert_eq!(held.version(), held_version);
    assert_eq!(held.hash(), &held_hash);
    assert!(
        !held
            .meta()
            .definition("widget")
            .is_some_and(|d| d.label.as_deref() == Some("Widget")),
        "the held snapshot never changes"
    );
    assert!(live.current().version() > held_version);
}

#[test]
fn adopt_never_goes_backwards_and_ignores_same_content() {
    let live = LiveMeta::at_version(site(), vec![widget()], spot_meta_live::Version::new(3))
        .unwrap_or_else(|e| panic!("builds: {e}"));
    let current = live.current();

    // The same version with the same content is a no-op.
    match live.adopt(current.version(), current.hash(), vec![widget()]) {
        Ok(spot_meta_live::Adopt::Unchanged) => {}
        other => panic!("expected Unchanged, got {other:?}"),
    }

    // An older version is ignored, never applied.
    match live.adopt(
        spot_meta_live::Version::new(1),
        current.hash(),
        vec![widget()],
    ) {
        Ok(spot_meta_live::Adopt::Stale) => {}
        other => panic!("expected Stale, got {other:?}"),
    }
    assert_eq!(live.current().version(), current.version());
}

#[test]
fn a_newer_version_with_a_mismatched_hash_is_divergence() {
    let live = LiveMeta::new(site(), vec![widget()]).unwrap_or_else(|e| panic!("builds: {e}"));
    let announced = widget_with("title");
    let announced_hash =
        MetaHash::of_definitions(&[announced]).unwrap_or_else(|e| panic!("hash: {e}"));

    // Version 5 built from different content does not hash to the announced hash.
    match live.adopt(
        spot_meta_live::Version::new(5),
        &announced_hash,
        vec![widget_with("name")],
    ) {
        Err(error) => {
            assert!(error.is_divergence(), "expected divergence, got {error:?}");
            assert_eq!(error.code(), "metalive.divergence");
        }
        Ok(adopt) => panic!("expected divergence, got {adopt:?}"),
    }
    assert_eq!(
        live.current().version(),
        spot_meta_live::Version::ZERO,
        "the node keeps its own snapshot"
    );
}
