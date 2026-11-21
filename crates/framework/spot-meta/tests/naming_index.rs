//! The naming backstop in the storage `Catalog` (WP-SEC-12.T2): a document that is named by a
//! `naming` pattern compiles to **one** unique index over its title column, under the name
//! `spot-plan` plans for it (`uq_<doctype>_<column>`, `spot-plan`'s `indexes::unique_index_name`).
//!
//! The plan already asks for that index; the plan is not what creates the database index — the
//! catalog compiled here is. Without it the uniqueness of a name exists only in the plan and in
//! the engine's name lock, and a writer that bypasses the engine (a migration, an import) can
//! store a second document under a taken name.
//!
//! The rule lives in both crates on purpose (no dependency between them), so these tests pin the
//! index names with literals, exactly as `spot-plan`'s unit tests do: if one side renames an
//! index, the other side's tests fail.
#![allow(missing_docs, clippy::expect_used)]

mod common;

use common::{build, of, simple, text};
use spot_app_api::prelude::Name;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_storage_contract::schema::{Catalog, Collation, EntityDef};

fn en() -> Collation {
    Collation::new("en").unwrap_or_else(|e| panic!("{e}"))
}

fn catalog(defs: Vec<Definition>) -> Catalog {
    build(defs)
        .unwrap_or_else(|e| panic!("{e:?}"))
        .catalog(en())
        .unwrap_or_else(|e| panic!("{e}"))
}

fn entity(cat: &Catalog, doctype: &str) -> EntityDef {
    cat.entity(&Name::new(doctype).unwrap_or_else(|e| panic!("{e}")))
        .unwrap_or_else(|e| panic!("{e}"))
        .clone()
}

/// `(name, unique)` for every index of `e`, in compiled order.
fn indexes(e: &EntityDef) -> Vec<(String, bool)> {
    e.indexes
        .iter()
        .map(|i| (i.name.to_string(), i.unique))
        .collect()
}

/// An `invoice` DocType with `fields`, a naming pattern and a title field.
fn titled(naming: Option<&str>, title: Option<&str>, fields: Vec<FieldSpec>) -> Definition {
    let mut d = simple("invoice", fields);
    d.naming = naming.map(str::to_owned);
    d.title_field = title.map(str::to_owned);
    d
}

#[test]
fn a_named_doctype_indexes_its_title_column_uniquely() {
    let cat = catalog(vec![titled(
        Some("INV-.#####"),
        Some("number"),
        vec![text("number")],
    )]);
    assert_eq!(
        indexes(&entity(&cat, "invoice")),
        vec![("uq_invoice_number".to_owned(), true)],
        "one unique index, the name `spot-plan` plans"
    );
}

#[test]
fn a_title_field_that_is_already_unique_still_gets_one_index() {
    let mut number = text("number");
    number.unique = true;
    let cat = catalog(vec![titled(
        Some("INV-.#####"),
        Some("number"),
        vec![number],
    )]);
    assert_eq!(
        indexes(&entity(&cat, "invoice")),
        vec![("uq_invoice_number".to_owned(), true)],
        "the field's own unique index and the backstop are the same index"
    );
}

#[test]
fn a_plainly_indexed_title_field_is_replaced_by_the_unique_index() {
    let mut number = text("number");
    number.index = true;
    let cat = catalog(vec![titled(
        Some("INV-.#####"),
        Some("number"),
        vec![number],
    )]);
    let ix = indexes(&entity(&cat, "invoice"));
    assert_eq!(ix, vec![("uq_invoice_number".to_owned(), true)]);
    assert!(
        ix.iter()
            .all(|(name, _)| !name.starts_with("ix_invoice_number")),
        "a table does not carry a plain index and a unique one over one column: {ix:?}"
    );
}

#[test]
fn a_doctype_without_naming_gets_no_title_index() {
    let cat = catalog(vec![titled(None, Some("number"), vec![text("number")])]);
    assert!(
        indexes(&entity(&cat, "invoice")).is_empty(),
        "no naming pattern, no unique name to keep"
    );
}

#[test]
fn naming_without_a_title_field_is_no_index_and_no_error() {
    // The engine refuses to name such a document at all
    // (`document.naming_title_field_missing`), so there is no name to keep unique — and
    // compiling one is not an error.
    let cat = catalog(vec![titled(Some("INV-.#####"), None, vec![text("number")])]);
    assert!(indexes(&entity(&cat, "invoice")).is_empty());
}

#[test]
fn a_child_table_gets_no_title_index() {
    let mut line = simple("line", vec![text("item")]);
    line.is_child = true;
    line.naming = Some("LINE-.#####".to_owned());
    line.title_field = Some("item".to_owned());
    let parent = simple("head", vec![of(FieldType::Table, "lines", Some("line"))]);
    let cat = catalog(vec![parent, line]);
    assert!(
        indexes(&entity(&cat, "line")).is_empty(),
        "rows of a parent's table carry no name of their own"
    );
}

#[test]
fn an_over_long_doctype_name_is_cut_the_way_the_plan_cuts_it() {
    // `compile::fit_name` and `spot-plan`'s `indexes::unique_index_name` must produce one name
    // for one index: `uq_<doctype>_<column>` cut to `Name::MAX_LEN` (63) bytes with
    // `_<fnv1a32 of the full name>` appended. This literal is that value for a 60-byte DocType
    // name, and it must stay in step with the `spot-plan` unit test
    // `indexes::tests::a_long_name_is_cut_and_hashed_within_the_limit` (same algorithm, same
    // limit) — a change on either side that changes a name fails here.
    let doctype = "a".repeat(60);
    let mut long = titled(Some("INV-.#####"), Some("title"), vec![text("title")]);
    long.name.clone_from(&doctype);
    let cat = catalog(vec![long]);
    let ix = indexes(&entity(&cat, &doctype));
    assert_eq!(ix.len(), 1, "{ix:?}");
    let expected = "uq_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_096b3eb9";
    assert_eq!(
        expected.len(),
        Name::MAX_LEN,
        "the pinned name is at the limit"
    );
    assert_eq!(ix[0].0, expected);
    assert!(ix[0].1, "the backstop is unique");
}
