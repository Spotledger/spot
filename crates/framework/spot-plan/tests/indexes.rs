//! The naming backstop (WP-DOC-05.T4): a DocType with a `naming` pattern has unique names, so
//! its plan marks the title field unique and asks the storage for a unique index on it — the
//! database then refuses a second row with the same name even when a writer skipped the
//! engine's name lock.
//!
//! Plan level only: the index these tests see is the one a schema planner creates. That the
//! PostgreSQL migration planner already builds it from a catalogue index, and refuses to build
//! a unique one over data that holds duplicates (leaving no invalid index behind on the next
//! run), is `spot-pg-migrate`'s own suite.
#![allow(missing_docs)]

mod common;

use std::sync::Arc;

use common::{effective, effective_from, of, text};
use spot_app_api::prelude::Name;
use spot_meta::{Definition, EffectiveMeta, FieldType};
use spot_plan::{DocPlan, PlanIndex, PlanSet};

/// The plan of the one DocType in `defs`.
fn plan_of(meta: &EffectiveMeta) -> Arc<DocPlan> {
    PlanSet::build(meta)
        .unwrap_or_else(|e| panic!("the definition set does not compile: {e:?}"))
        .get(&meta.definitions()[0].name)
        .unwrap_or_else(|| panic!("no plan for the only DocType"))
}

/// A DocType named `doc` whose name is its `title` field, which is `unique` when asked for.
fn naming(title: &str, unique: bool) -> Definition {
    let mut def = Definition::new("doc");
    def.title_field = Some(title.to_owned());
    def.naming = Some(format!("field:{title}"));
    let mut field = text(title);
    field.unique = unique;
    def.fields = vec![field, text("memo")];
    def
}

/// The same DocType without a naming pattern.
fn unnamed(title: &str, unique: bool) -> Definition {
    let mut def = naming(title, unique);
    def.naming = None;
    def
}

/// The planned index names of a plan.
fn index_names(plan: &DocPlan) -> Vec<&str> {
    plan.unique_indexes().iter().map(PlanIndex::name).collect()
}

#[test]
fn a_naming_doctype_marks_its_title_unique_and_plans_one_index() {
    let meta = effective_from(vec![naming("subject", false)]);
    let plan = plan_of(&meta);
    // The definition never says `unique`; the plan says it for the definition, because a
    // naming DocType's names are unique.
    assert!(
        plan.field("subject").is_some_and(|f| f.unique),
        "the title field of a naming DocType is unique in the plan"
    );
    assert_eq!(index_names(&plan), ["uq_doc_subject"]);
    let index = plan
        .unique_indexes()
        .first()
        .unwrap_or_else(|| panic!("no index"));
    assert_eq!(index.column().as_str(), "subject");
    // The other field is untouched.
    assert!(plan.field("memo").is_some_and(|f| !f.unique));
}

#[test]
fn a_title_field_that_is_already_unique_gets_one_index_not_two() {
    let meta = effective_from(vec![naming("subject", true)]);
    let plan = plan_of(&meta);
    assert_eq!(
        index_names(&plan),
        ["uq_doc_subject"],
        "one index, not a field index plus a backstop"
    );
}

#[test]
fn the_backstop_is_the_same_index_a_unique_field_gets() {
    let configured = plan_of(&effective_from(vec![naming("subject", true)]));
    let derived = plan_of(&effective_from(vec![naming("subject", false)]));
    assert_eq!(
        index_names(&configured),
        index_names(&derived),
        "an author marking the title unique and the naming backstop name one index, so a \
         metadata change that drops `unique` from a naming title renames nothing"
    );
}

#[test]
fn a_doctype_without_naming_gets_no_index() {
    let meta = effective_from(vec![unnamed("subject", false)]);
    let plan = plan_of(&meta);
    assert!(plan.unique_indexes().is_empty());
    assert!(
        plan.field("subject").is_some_and(|f| !f.unique),
        "a title that several documents may share stays not unique"
    );
    // A unique field of a DocType without naming is still indexed as before.
    let plan = plan_of(&effective_from(vec![unnamed("subject", true)]));
    assert_eq!(index_names(&plan), ["uq_doc_subject"]);
}

#[test]
fn a_naming_doctype_without_a_title_field_gets_no_index() {
    let mut def = Definition::new("doc");
    def.naming = Some("hash".into());
    def.fields = vec![text("subject")];
    let plan = plan_of(&effective_from(vec![def]));
    assert!(
        plan.unique_indexes().is_empty(),
        "the engine refuses to name such a document, so there is no name to keep unique"
    );
}

#[test]
fn a_child_table_gets_no_naming_backstop() {
    let mut child = Definition::new("doc_line");
    child.is_child = true;
    child.title_field = Some("label".into());
    child.naming = Some("field:label".into());
    child.fields = vec![text("label")];
    let mut parent = Definition::new("doc");
    parent.fields = vec![of(FieldType::Table, "lines", Some("doc_line"))];
    let meta = effective_from(vec![parent, child]);
    let set = PlanSet::build(&meta).unwrap_or_else(|e| panic!("no plans: {e:?}"));
    let line = set
        .get("doc_line")
        .unwrap_or_else(|| panic!("no child plan"));
    assert!(
        line.unique_indexes().is_empty(),
        "rows of a parent's table carry no name of their own"
    );
}

#[test]
fn the_planned_index_names_the_doctype_and_the_field_it_protects() {
    let plan = plan_of(&effective_from(vec![naming("subject", false)]));
    let index = plan
        .unique_indexes()
        .first()
        .unwrap_or_else(|| panic!("no index"));
    assert_eq!(index.column().as_str(), "subject");
    assert_eq!(index.name(), format!("uq_{}_subject", plan.name().as_str()));
}

#[test]
fn the_index_name_is_stable_across_two_builds() {
    let once = plan_of(&effective_from(vec![naming("subject", false)]));
    let twice = plan_of(&effective_from(vec![naming("subject", false)]));
    assert_eq!(index_names(&once), index_names(&twice));
    assert_eq!(once.plan_hash(), twice.plan_hash());
}

#[test]
fn the_index_name_fits_the_identifier_limit_for_a_long_doctype_name() {
    let long = "a".repeat(60);
    let mut def = naming("subject", false);
    def.name = long.clone();
    let plan = plan_of(&effective_from(vec![def]));
    let name = index_names(&plan)
        .first()
        .copied()
        .unwrap_or_else(|| panic!("no index"));
    // 3 + 60 + 1 + 7 bytes would be 71: cut to the limit and made unique by a hash suffix.
    assert_eq!(name.len(), Name::MAX_LEN, "{name}");
    assert!(name.starts_with("uq_"), "{name}");
    assert!(
        name[Name::MAX_LEN - 9..].starts_with('_'),
        "the hash suffix: {name}"
    );
    assert!(
        name[Name::MAX_LEN - 8..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit()),
        "the suffix is hex: {name}"
    );
    assert!(
        Name::new(name).is_ok(),
        "the fitted name is a valid storage name: {name}"
    );
    // Two long DocTypes do not collide on one index name.
    let mut other = naming("subject", false);
    other.name = "a".repeat(59);
    let other_plan = plan_of(&effective_from(vec![other]));
    assert_ne!(index_names(&other_plan), [name]);
}

#[test]
fn every_planned_index_name_is_a_valid_storage_name() {
    // The fixture set (a posted, naming `invoice` with a unique title) plus a fitted name.
    let mut long = naming("subject", false);
    long.name = "a".repeat(60);
    let mut defs = common::definitions();
    defs.push(long);
    let set = PlanSet::build(&effective_from(defs))
        .unwrap_or_else(|e| panic!("the fixtures do not compile: {e:?}"));
    let mut seen = 0;
    for doctype in set.doctypes() {
        let plan = set.get(doctype).unwrap_or_else(|| panic!("no {doctype}"));
        for index in plan.unique_indexes() {
            assert!(
                Name::new(index.name()).is_ok(),
                "{doctype}: `{}` is not a storage name",
                index.name()
            );
            assert!(
                index.name().starts_with(&format!("uq_{doctype}_"))
                    || (index.name().len() == Name::MAX_LEN && index.name().starts_with("uq_")),
                "{doctype}: `{}` is named after its table",
                index.name()
            );
            assert!(
                plan.field(index.column().as_str()).is_some(),
                "{doctype}: `{}` indexes no planned field",
                index.column()
            );
            seen += 1;
        }
    }
    assert!(seen >= 2, "the fixture set plans unique indexes");
}

#[test]
fn the_fixture_invoice_keeps_its_title_index() {
    let set = PlanSet::build(&effective()).unwrap_or_else(|e| panic!("no plans: {e:?}"));
    let invoice = set
        .get("invoice")
        .unwrap_or_else(|| panic!("no invoice plan"));
    assert_eq!(index_names(&invoice), ["uq_invoice_number"]);
    assert_eq!(invoice.unique_indexes()[0].column().as_str(), "number");
}
