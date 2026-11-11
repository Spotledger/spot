//! The self-test harness over every seed remedy: each ships a scenario and passes it.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use spot_remedies::{Registry, remedy_test};

/// Every seed remedy ships a real scenario and passes precondition, dry-run, apply, its
/// invariants and undo.
#[test]
fn every_seed_remedy_passes_its_scenario() {
    let registry = Registry::seeds().unwrap();
    assert_eq!(registry.len(), 7, "the seven seed remedies are registered");

    for id in registry.ids() {
        let remedy = registry.get(id).unwrap();
        let spec = remedy.self_test();
        let world = &spec.scenario.world;
        let seeded = world.jobs.len()
            + world.cache.len()
            + world.indexes.len()
            + world.edges.len()
            + world.quarantine.len()
            + world.cases.len()
            + world.documents.len();
        assert!(
            seeded > 0,
            "{id} ships a scenario with no fault in its fixture"
        );
        assert!(
            !spec.scenario.name.trim().is_empty(),
            "{id} ships an unnamed scenario"
        );

        let report =
            remedy_test::run(&registry, id, &spec).unwrap_or_else(|failure| panic!("{failure}"));
        assert_eq!(&report.remedy, id);
        assert!(report.plan_steps > 0, "{id} planned no steps");
        assert!(report.changes > 0, "{id} applied no changes");
    }
}

/// The registry test enumerates the remedies; a registry with a remedy that has no scenario cannot
/// exist, because the trait forces one. This proves the enumeration reaches every remedy.
#[test]
fn the_harness_runs_exactly_the_registered_remedies() {
    let registry = Registry::seeds().unwrap();
    let mut ran = Vec::new();
    for id in registry.ids() {
        let spec = registry.get(id).unwrap().self_test();
        let report = remedy_test::run(&registry, id, &spec).unwrap();
        ran.push(report.remedy);
    }
    assert_eq!(ran, registry.ids());
}
