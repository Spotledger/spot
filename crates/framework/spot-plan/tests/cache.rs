//! The plan set and the plan cache: building every plan at once, refresh keyed on the
//! metadata version, Arc reuse by plan hash, and a concurrent reader's consistent view while
//! another thread refreshes (std::thread, no sleeps).
#![allow(missing_docs)]

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use common::registry;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_plan::{PlanCache, PlanSet};

fn text_custom(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// The invoice's field count at metadata version `version` (the fixtures start at version 1
/// with 13 declaration-order fields; the writer adds one custom field per version bump).
fn invoice_fields_at(version: u64) -> usize {
    // +2: the engine-managed fields of the posted DocType.
    15 + (version - 1) as usize + 2
}

fn party_fields_at(_version: u64) -> usize {
    11
}

#[test]
fn the_set_carries_every_doctype_and_the_version() {
    let reg = registry();
    let meta = reg.effective();
    let set = PlanSet::build(&meta).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    assert_eq!(set.meta_version(), meta.version());
    let names: Vec<&str> = set.doctypes().collect();
    assert_eq!(names, ["file", "invoice", "invoice_line", "party", "pick"]);
    assert!(set.get("invoice").is_some());
    assert!(set.get("nope").is_none());
}

#[test]
fn refresh_reports_exactly_the_changed_doctypes_and_reuses_the_unchanged_arcs() {
    let mut reg = registry();
    let cache = PlanCache::new(
        PlanSet::build(&reg.effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}")),
    );
    let party_before = cache
        .current()
        .get("party")
        .unwrap_or_else(|| panic!("no party"));
    let invoice_before = cache
        .current()
        .get("invoice")
        .unwrap_or_else(|| panic!("no invoice"));

    // An unchanged version: nothing rebuilds, nothing changed.
    let unchanged = cache
        .refresh(&reg.effective())
        .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
    assert!(
        unchanged.is_empty(),
        "no rebuild when the version did not move"
    );
    assert!(
        Arc::ptr_eq(
            &cache
                .current()
                .get("party")
                .unwrap_or_else(|| panic!("no party")),
            &party_before
        ),
        "an unchanged refresh reuses every Arc"
    );

    // One custom field on the invoice: only the invoice's plan changes.
    reg.add_custom_field("invoice", text_custom("custom_score"), None)
        .unwrap_or_else(|e| panic!("the custom field is refused: {e:?}"));
    let changed = cache
        .refresh(&reg.effective())
        .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
    assert_eq!(changed, ["invoice"]);
    let set = cache.current();
    assert_eq!(set.meta_version(), 2);
    let party_after = set.get("party").unwrap_or_else(|| panic!("no party"));
    assert!(
        Arc::ptr_eq(&party_after, &party_before),
        "the unchanged DocType's Arc is reused, pointer-equal"
    );
    let invoice_after = set.get("invoice").unwrap_or_else(|| panic!("no invoice"));
    assert!(!Arc::ptr_eq(&invoice_after, &invoice_before));
    assert!(
        invoice_after.field("custom_score").is_some(),
        "the new plan contains the added custom field"
    );
    assert_eq!(
        invoice_after.field_ix("custom_score"),
        Some(spot_plan::FieldIx::new(15)),
        "appended at the end of the 15 declared fields"
    );

    // A second custom field: the invoice changes again, everything else stays.
    reg.add_custom_field("invoice", text_custom("custom_memo"), Some("custom_score"))
        .unwrap_or_else(|e| panic!("the custom field is refused: {e:?}"));
    let changed = cache
        .refresh(&reg.effective())
        .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
    assert_eq!(changed, ["invoice"]);
    assert!(
        Arc::ptr_eq(
            &cache
                .current()
                .get("party")
                .unwrap_or_else(|| panic!("no party")),
            &party_before
        ),
        "an unchanged DocType keeps its Arc across several refreshes"
    );
    assert!(
        cache
            .current()
            .get("invoice")
            .unwrap_or_else(|| panic!("no invoice"))
            .field("custom_memo")
            .is_some()
    );
}

#[test]
fn refresh_picks_up_new_and_gone_doctypes() {
    let mut reg = registry();
    let cache = PlanCache::new(
        PlanSet::build(&reg.effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}")),
    );
    // A fresh standard update with one more DocType: `set_definitions` bumps the version.
    let mut defs = common::definitions();
    defs.push(Definition::new("widget"));
    reg.set_definitions(defs)
        .unwrap_or_else(|e| panic!("the definitions are refused: {e:?}"));
    let changed = cache
        .refresh(&reg.effective())
        .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
    assert_eq!(changed, ["widget"], "a new DocType's plan is new");
    assert!(cache.current().get("widget").is_some());

    // And a DocType gone from the metadata is gone from the set.
    let mut defs = common::definitions();
    defs.push(Definition::new("widget2"));
    reg.set_definitions(defs)
        .unwrap_or_else(|e| panic!("the definitions are refused: {e:?}"));
    let changed = cache
        .refresh(&reg.effective())
        .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
    assert_eq!(changed, ["widget2"], "widget was replaced by widget2");
    assert!(cache.current().get("widget").is_none());
    assert!(cache.current().get("widget2").is_some());

    // Every DocType from the fixtures plus widget2 has a plan; widget is gone.
    let set =
        PlanSet::build(&reg.effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    assert_eq!(set.doctypes().collect::<BTreeSet<_>>().len(), 6);
}

#[test]
fn a_concurrent_reader_keeps_a_consistent_set_while_another_refreshes() {
    let mut reg = registry();
    let base_version = 1;
    let base =
        PlanSet::build(&reg.effective()).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    assert_eq!(base.meta_version(), base_version);
    let cache = Arc::new(PlanCache::new(base));

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<String>();
    let reader_cache = Arc::clone(&cache);
    let reader_stop = Arc::clone(&stop);
    let reader = thread::spawn(move || {
        // No sleeps: read as fast as the thread can, checking the invariant every read, until
        // the writer says it has stopped refreshing. A violated invariant means the reader saw
        // a set that does not explain itself (a half-built set).
        while !reader_stop.load(Ordering::SeqCst) {
            let set = reader_cache.current();
            let version = set.meta_version();
            // The set's own version must explain its own content: every version bump added
            // exactly one custom field to the invoice, and the party's plan never changes.
            let invoice = match set.get("invoice") {
                Some(p) => p,
                None => {
                    tx.send("no invoice plan in the set".into()).ok();
                    break;
                }
            };
            if invoice.fields().len() != invoice_fields_at(version) {
                tx.send(format!(
                    "invoice has {} fields at version {version}, expected {}",
                    invoice.fields().len(),
                    invoice_fields_at(version)
                ))
                .ok();
            }
            match set.get("party") {
                Some(party) if party.fields().len() == party_fields_at(version) => {
                    // Consistent with the version, so the reader saw no half-built set.
                }
                Some(party) => {
                    tx.send(format!(
                        "party has {} fields at version {version}",
                        party.fields().len()
                    ))
                    .ok();
                }
                None => {
                    tx.send("no party plan in the set".into()).ok();
                    break;
                }
            }
        }
    });

    // The writer: refreshes, each adding one custom field (the version rises by one).
    for i in 0..16 {
        reg.add_custom_field("invoice", text_custom(&format!("custom_{i}")), None)
            .unwrap_or_else(|e| panic!("the custom field is refused: {e:?}"));
        let changed = cache
            .refresh(&reg.effective())
            .unwrap_or_else(|e| panic!("the refresh failed: {e:?}"));
        assert_eq!(changed, ["invoice"]);
    }
    stop.store(true, Ordering::SeqCst);
    reader
        .join()
        .unwrap_or_else(|e| panic!("the reader panicked: {e:?}"));
    // The reader never saw a half-built set.
    let mut violations: Vec<String> = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        violations.push(msg);
    }
    assert!(
        violations.is_empty(),
        "every read was internally consistent: {violations:?}"
    );
    assert_eq!(cache.current().meta_version(), base_version + 16);
}

#[test]
fn refresh_of_the_same_metadata_changes_no_plan_and_reuses_every_arc() {
    // The version is not a cache key: a refresh always rebuilds the set, and the plan hashes
    // decide what is reused. Unchanged metadata therefore changes nothing, at the cost of one
    // build, and every plan stays the same `Arc`.
    let reg = registry();
    let meta = reg.effective();
    let set = PlanSet::build(&meta).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
    let cache = PlanCache::new(set);
    let before: Vec<_> = cache
        .current()
        .doctypes()
        .map(|name| {
            cache
                .current()
                .get(name)
                .unwrap_or_else(|| panic!("no plan for {name}"))
        })
        .collect();
    let changed = cache
        .refresh(&meta)
        .unwrap_or_else(|e| panic!("refresh failed: {e:?}"));
    assert!(changed.is_empty(), "identical metadata changes no plan");
    let after = cache.current();
    assert_eq!(after.meta_version(), meta.version());
    assert_eq!(after.doctypes().count(), before.len());
    for (name, old) in after.doctypes().zip(&before) {
        let new = after
            .get(name)
            .unwrap_or_else(|| panic!("no plan for {name}"));
        assert!(
            Arc::ptr_eq(&new, old),
            "{name}'s plan is reused, pointer-equal"
        );
    }
}
