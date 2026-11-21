//! Retention decisions: legal holds, statutory floors, exceptions and referential safety — in the
//! plan and again when the executor runs it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_data_policy::{
    ExecutionOptions, FieldClassMap, Gregorian, Holds, MemoryStore, PolicySet, Resume,
    RetentionStore, SkipReason, StatutoryFloor, execute, plan_run,
};

fn store(records: Vec<spot_data_policy::Record>) -> MemoryStore {
    MemoryStore::from_records(records).unwrap()
}

fn deleting_notes() -> spot_data_policy::RetentionRules {
    common::rules(&[("notes", "note", "created", "1y", "delete")])
}

#[test]
fn a_legal_hold_prevents_deletion_in_the_plan() {
    let store = store(vec![common::record(
        "note-1",
        "note",
        common::at(2020, 1, 1),
    )]);
    let rules = deleting_notes();
    let holds = Holds::from_toml(
        "[[hold]]\nid = \"case-7\"\nreason = \"litigation\"\nset_by = \"user:legal\"\nscope = { records = [\"note-1\"] }\n",
    )
    .unwrap();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert!(plan.items().is_empty(), "a hold suspends every action");
    let skip = plan
        .skips()
        .iter()
        .find(|skip| skip.record_id == "note-1")
        .unwrap();
    assert_eq!(
        skip.reason,
        SkipReason::LegalHold {
            hold: "case-7".to_owned(),
            reason: "litigation".to_owned(),
        }
    );
    assert_eq!(plan.count("notes").unwrap().skipped, 1);
}

#[test]
fn a_hold_set_after_planning_still_stops_deletion_at_execution() {
    let mut store = store(vec![common::record(
        "note-1",
        "note",
        common::at(2020, 1, 1),
    )]);
    let rules = deleting_notes();
    let no_holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &no_holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert_eq!(plan.items().len(), 1);

    let holds = Holds::from_toml(
        "[[hold]]\nid = \"late-hold\"\nreason = \"preserve\"\nset_by = \"user:legal\"\nscope = \"all\"\n",
    )
    .unwrap();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let options = ExecutionOptions {
        dry_run: false,
        ..ExecutionOptions::default()
    };
    let report = execute(
        &plan,
        &policy,
        &mut store,
        &options,
        Resume::Fresh,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert!(report.completed);
    assert_eq!(report.applied(), 0);
    assert_eq!(report.skipped(), 1);
    assert_eq!(
        report.outcomes[0].reason,
        Some(SkipReason::LegalHold {
            hold: "late-hold".to_owned(),
            reason: "preserve".to_owned(),
        })
    );
    assert!(
        store.record("note-1").unwrap().is_some(),
        "nothing was deleted"
    );
    assert!(store.proofs().unwrap().is_empty());
}

#[test]
fn the_statutory_floor_wins_over_a_shorter_rule() {
    let store = store(vec![common::classified(
        "invoice-1",
        common::at(2020, 1, 1),
        "financial",
        "de",
    )]);
    let rules = common::rules(&[("invoices", "sales_invoice", "created", "2y", "delete")]);
    let holds = Holds::empty();
    let floors = StatutoryFloor::from_toml(
        "[[floor]]\ndoc_class = \"financial\"\njurisdiction = \"de\"\nperiod = \"10y\"\n",
    )
    .unwrap();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);

    // Five years on: the two-year rule is long past, the ten-year floor is not.
    let plan = plan_run(&policy, &store, common::at(2025, 1, 1)).unwrap();
    assert!(plan.items().is_empty(), "the floor blocks the earlier rule");
    assert_eq!(
        plan.skips()[0].reason,
        SkipReason::StatutoryFloorPending {
            earliest: common::at(2030, 1, 1)
        }
    );

    // Eleven years on: both are past and the deletion is planned, dated by the floor.
    let plan = plan_run(&policy, &store, common::at(2031, 1, 1)).unwrap();
    assert_eq!(plan.items().len(), 1);
    assert_eq!(plan.items()[0].due_at, common::at(2030, 1, 1));
    assert!(plan.items()[0].reason.contains("statutory floor"));
}

#[test]
fn an_unknown_jurisdiction_is_never_deleted() {
    let store = store(vec![common::classified(
        "invoice-1",
        common::at(2020, 1, 1),
        "financial",
        "fr",
    )]);
    let rules = common::rules(&[("invoices", "sales_invoice", "created", "1y", "delete")]);
    let holds = Holds::empty();
    let floors = StatutoryFloor::from_toml(
        "[[floor]]\ndoc_class = \"financial\"\njurisdiction = \"de\"\nperiod = \"10y\"\n",
    )
    .unwrap();
    let fields = FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert!(plan.items().is_empty());
    assert_eq!(plan.skips()[0].reason, SkipReason::StatutoryFloorUnknown);
}

#[test]
fn a_record_still_linked_by_a_retained_record_is_skipped_with_the_referrers() {
    let mut parent = common::record("parent", "note", common::at(2025, 6, 1));
    parent.references.push("child".to_owned());
    let child = common::record("child", "note", common::at(2020, 1, 1));
    let store = store(vec![parent, child]);
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert!(
        plan.items().is_empty(),
        "the child is linked by a retained parent"
    );
    let skip = plan
        .skips()
        .iter()
        .find(|skip| skip.record_id == "child")
        .unwrap();
    assert_eq!(
        skip.reason,
        SkipReason::Referential {
            referrers: vec!["parent".to_owned()]
        }
    );
}

#[test]
fn two_records_planned_together_do_not_block_each_other() {
    let mut parent = common::record("parent", "note", common::at(2020, 1, 1));
    parent.references.push("child".to_owned());
    let child = common::record("child", "note", common::at(2020, 1, 1));
    let store = store(vec![parent, child]);
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert_eq!(
        plan.items().len(),
        2,
        "both are deleted, so neither is retained"
    );
}

#[test]
fn an_exception_excludes_a_record_from_its_rule() {
    let store = store(vec![
        common::record("keep", "note", common::at(2020, 1, 1)),
        common::record("drop", "note", common::at(2020, 1, 1)),
    ]);
    let rules = spot_data_policy::RetentionRules::from_toml(
        "[[rule]]\nid = \"notes\"\ndoctype = \"note\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\nexceptions = { record_ids = [\"keep\"] }\n",
    )
    .unwrap();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert_eq!(plan.items().len(), 1);
    assert_eq!(plan.items()[0].record_id, "drop");
    assert_eq!(plan.skips()[0].reason, SkipReason::Exception);
}
