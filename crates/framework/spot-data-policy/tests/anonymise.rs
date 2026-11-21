//! Anonymisation: the metadata classification decides what is replaced, ledger totals survive, and
//! the audit trail names no field value.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_data_policy::{
    ExecutionOptions, FieldValue, Gregorian, Holds, MemoryStore, PolicySet, Resume, RetentionStore,
    SkipReason, StatutoryFloor, execute, plan_run,
};

#[test]
fn anonymisation_replaces_personal_fields_and_keeps_ledger_totals() {
    let mut store =
        MemoryStore::from_records(vec![common::invoice("invoice-1", common::at(2020, 1, 1))])
            .unwrap();
    let rules = common::rules(&[("invoices", "sales_invoice", "created", "1y", "anonymise")]);
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = common::fields();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let plan = plan_run(&policy, &store, common::at(2026, 1, 1)).unwrap();

    assert_eq!(plan.items().len(), 1);
    assert_eq!(
        plan.items()[0].anonymise_fields,
        vec!["customer_name".to_owned()],
        "customer_ref is ledger-relevant and total is not personal"
    );

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
    assert_eq!(report.applied(), 1);

    let record = store.record("invoice-1").unwrap().unwrap();
    assert_eq!(
        record.fields.get("customer_name"),
        Some(&FieldValue::Text("[anonymised]".to_owned()))
    );
    assert_eq!(
        record.fields.get("customer_ref"),
        Some(&FieldValue::Text("CUST-7".to_owned())),
        "a ledger-relevant reference is kept"
    );
    assert_eq!(
        record.fields.get("total"),
        Some(&FieldValue::Money(12_345)),
        "the total is untouched"
    );
    assert_eq!(
        record.fields.get("note"),
        Some(&FieldValue::Text("keep me".to_owned())),
        "a public field is untouched"
    );
    assert!(
        store.proofs().unwrap().is_empty(),
        "anonymisation is not a deletion"
    );
    assert!(
        store
            .effects()
            .iter()
            .any(|effect| effect.effect == "anonymise")
    );
}

#[test]
fn the_audit_record_names_no_field_value() {
    let mut store =
        MemoryStore::from_records(vec![common::invoice("invoice-1", common::at(2020, 1, 1))])
            .unwrap();
    let rules = common::rules(&[("invoices", "sales_invoice", "created", "1y", "anonymise")]);
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = common::fields();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let plan = plan_run(&policy, &store, common::at(2026, 1, 1)).unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        actor: "user:dpo".to_owned(),
        ..ExecutionOptions::default()
    };
    execute(
        &plan,
        &policy,
        &mut store,
        &options,
        Resume::Fresh,
        common::at(2026, 1, 1),
    )
    .unwrap();

    let audit = &store.audits()[0];
    assert_eq!(audit.kind, "data_policy.anonymise");
    assert_eq!(audit.actor, "user:dpo");
    assert_eq!(audit.subject.as_ref().unwrap().id, "invoice-1");
    let rendered = format!("{:?}", audit.payload);
    assert!(
        !rendered.contains("Ada Lovelace"),
        "no personal value in audit"
    );
    assert!(!rendered.contains("CUST-7"), "no reference value in audit");
    assert!(!rendered.contains("12_345"), "no total in audit");
}

#[test]
fn an_anonymisation_rule_without_classification_is_skipped_not_guessed() {
    let store =
        MemoryStore::from_records(vec![common::invoice("invoice-1", common::at(2020, 1, 1))])
            .unwrap();
    let rules = common::rules(&[("invoices", "sales_invoice", "created", "1y", "anonymise")]);
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = spot_data_policy::FieldClassMap::default();
    let plan = plan_run(
        &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
        &store,
        common::at(2026, 1, 1),
    )
    .unwrap();
    assert!(plan.items().is_empty());
    assert_eq!(plan.skips()[0].reason, SkipReason::Unclassified);
}
