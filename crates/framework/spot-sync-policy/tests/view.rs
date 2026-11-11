//! The excluded-field view: a field never leaves the origin, and cannot be received.
//!
//! `SyncPolicy::excludes` is a rule **by name**, wherever the name appears: a parent field, a
//! whole child table, or a field inside a child row. The tests below pin all three levels, and the
//! property test pins that what the sender builds is always what the receiver accepts.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::ChildRow;
use spot_app_api::Document;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_sync_policy::{
    Class, Direction, FieldName, SyncPolicy, Trigger, accept_incoming, codes, outgoing_view,
};

fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

fn policy() -> SyncPolicy {
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        BTreeSet::from([FieldName::new("cost_price").unwrap()]),
    )
}

fn document() -> Document {
    let mut doc = Document::new(name("item"), RecordId::new_v7());
    doc.set(name("title"), Value::Text("Widget".to_owned()));
    doc.set(name("cost_price"), Value::Text("9.99".to_owned()));
    doc
}

/// A policy that excludes the names given, whatever level they appear at.
fn excluding(names: &[&str]) -> SyncPolicy {
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        names.iter().map(|n| FieldName::new(n).unwrap()).collect(),
    )
}

/// A deterministic row id: a UUIDv7 whose last byte is the row number, so a test failure names the
/// row and ids are reproducible.
fn row_id(n: u8) -> RecordId {
    let mut bytes = [7u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70 | (n & 0x0f);
    bytes[8] = 0x80;
    bytes[15] = n;
    RecordId::from_bytes(bytes).unwrap()
}

fn row(n: u8, fields: &[(&str, &str)]) -> ChildRow {
    ChildRow {
        id: row_id(n),
        fields: fields
            .iter()
            .map(|(field, value)| (name(field), Value::Text((*value).to_owned())))
            .collect(),
    }
}

/// An `invoice` with a `lines` table whose rows carry `margin` (excluded) and `qty` (kept).
fn invoiced() -> Document {
    let mut doc = Document::new(name("invoice"), RecordId::new_v7());
    doc.set(name("title"), Value::Text("Invoice".to_owned()));
    doc.set(name("notes"), Value::Text("net 30".to_owned()));
    let lines = doc.rows_mut(name("lines"));
    lines.push(row(1, &[("qty", "2"), ("margin", "1.50")]));
    lines.push(row(2, &[("qty", "5"), ("margin", "0.25")]));
    lines.push(row(3, &[("qty", "1"), ("margin", "4.00")]));
    doc
}

/// An `invoice` with one line, whose only excluded name sits inside the row.
fn one_leaking_row() -> Document {
    let mut doc = Document::new(name("invoice"), RecordId::new_v7());
    doc.set(name("title"), Value::Text("Invoice".to_owned()));
    doc.rows_mut(name("lines"))
        .push(row(1, &[("qty", "2"), ("margin", "1.50")]));
    doc
}

#[test]
fn an_excluded_field_is_absent_from_the_outgoing_view() {
    let view = outgoing_view(&document(), &policy());
    let names: Vec<&str> = view.fields().keys().map(Name::as_str).collect();
    assert!(names.contains(&"title"), "kept field is present: {names:?}");
    assert!(
        !names.contains(&"cost_price"),
        "excluded field must be absent, not blanked: {names:?}"
    );
}

#[test]
fn a_document_that_still_carries_an_excluded_field_cannot_be_received() {
    let problems = accept_incoming("item", &document(), &policy()).unwrap_err();
    assert!(
        problems
            .iter()
            .any(|p| p.code() == codes::EXCLUDED_FIELD_RECEIVED),
        "got {problems:?}"
    );
    assert!(problems.iter().any(|p| p.detail().contains("cost_price")));
}

#[test]
fn a_view_without_the_excluded_field_is_accepted() {
    let view = outgoing_view(&document(), &policy());
    assert!(accept_incoming("item", &view, &policy()).is_ok());
}

// ---- (a) an excluded name is removed inside child rows too ----

#[test]
fn an_excluded_name_is_removed_from_child_rows_and_the_kept_one_stays() {
    let origin = invoiced();
    let view = outgoing_view(&origin, &excluding(&["margin"]));

    let rows = view.rows("lines");
    assert_eq!(rows.len(), 3, "every row is kept: {rows:?}");
    for (seen, source) in rows.iter().zip(origin.rows("lines")) {
        assert_eq!(seen.id, source.id, "row ids are preserved, in order");
        let fields: Vec<&str> = seen.fields.keys().map(Name::as_str).collect();
        assert_eq!(
            fields,
            vec!["qty"],
            "only the kept field remains: {fields:?}"
        );
        assert_eq!(
            seen.fields.get(&name("qty")),
            source.fields.get(&name("qty"))
        );
    }
    let ids: Vec<RecordId> = rows.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![row_id(1), row_id(2), row_id(3)],
        "row order holds"
    );
}

#[test]
fn a_row_field_with_an_excluded_name_is_not_shipped() {
    let view = outgoing_view(&invoiced(), &excluding(&["margin"]));
    for row in view.rows("lines") {
        for value in row.fields.values() {
            assert_ne!(
                value,
                &Value::Text("1.50".to_owned()),
                "the excluded value must not survive under any name"
            );
        }
    }
}

// ---- (b) a whole excluded table stays absent ----

#[test]
fn an_excluded_table_is_still_absent_even_though_its_rows_are_cleaned() {
    let view = outgoing_view(&invoiced(), &excluding(&["lines"]));
    assert!(
        view.tables().is_empty(),
        "an excluded table is absent, not empty: {:?}",
        view.tables()
    );
    assert!(view.rows("lines").is_empty());
}

// ---- (c) a table whose every field is excluded keeps its rows ----

#[test]
fn a_table_whose_every_field_is_excluded_keeps_its_rows_with_empty_field_maps() {
    let origin = invoiced();
    // Only names the rows carry are excluded, so every row loses every field it has.
    let view = outgoing_view(&origin, &excluding(&["qty", "margin"]));
    let rows = view.rows("lines");
    // Dropping a row would change what the receiver believes the document is: three lines with
    // three ids is the document's shape, and losing a row silently is a worse outcome than an
    // empty row. So the rows stay, in order, with empty field maps.
    assert_eq!(
        rows.len(),
        origin.rows("lines").len(),
        "row count matches the origin: {rows:?}"
    );
    let ids: Vec<RecordId> = rows.iter().map(|r| r.id).collect();
    let origin_ids: Vec<RecordId> = origin.rows("lines").iter().map(|r| r.id).collect();
    assert_eq!(ids, origin_ids, "row ids match the origin, in order");
    for row in rows {
        assert!(
            row.fields.is_empty(),
            "every field is excluded, so the map is empty: {:?}",
            row.fields
        );
    }
}

// ---- (d) the receiver refuses a row that carries an excluded name ----

#[test]
fn a_document_whose_row_carries_an_excluded_field_cannot_be_received() {
    let problems =
        accept_incoming("invoice", &one_leaking_row(), &excluding(&["margin"])).unwrap_err();
    assert_eq!(
        problems.len(),
        1,
        "one problem names the child-row leak: {problems:?}"
    );
    let problem = &problems[0];
    assert_eq!(problem.code(), codes::EXCLUDED_FIELD_RECEIVED);
    assert_eq!(problem.doctype(), "invoice");
    let detail = problem.detail();
    assert!(detail.contains("lines"), "names the table: {detail}");
    assert!(detail.contains("margin"), "names the field: {detail}");
    let first = row_id(1).to_string();
    assert!(
        detail.contains(&first),
        "names the row id {first}: {detail}"
    );
    assert!(
        !detail.contains("1.50"),
        "the message names the field, never its value: {detail}"
    );
}

#[test]
fn every_offending_row_field_is_named() {
    let problems = accept_incoming("invoice", &invoiced(), &excluding(&["margin"])).unwrap_err();
    let details: Vec<&str> = problems.iter().map(|p| p.detail()).collect();
    for n in 1..=3u8 {
        let id = row_id(n).to_string();
        assert!(
            details.iter().any(|d| d.contains(&id)),
            "row {id} is named: {details:?}"
        );
    }
}

// ---- (e) a clean document passes ----

#[test]
fn a_clean_document_passes_the_receiver_guard() {
    let clean = outgoing_view(&invoiced(), &excluding(&["margin"]));
    assert!(
        accept_incoming("invoice", &clean, &excluding(&["margin"])).is_ok(),
        "the view of a document is always receivable: {:?}",
        clean.tables()
    );
}

// ---- (f) property: the view a sender builds is always accepted ----

/// A small deterministic generator, so a failing seed reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

const PARENT_FIELDS: [&str; 4] = ["title", "cost_price", "notes", "margin"];
const TABLES: [&str; 3] = ["lines", "taxes", "comments"];
const ROW_FIELDS: [&str; 3] = ["qty", "margin", "rate"];

fn random_document(rng: &mut Lcg) -> Document {
    let mut doc = Document::new(name("invoice"), RecordId::new_v7());
    for field in PARENT_FIELDS {
        if rng.chance(2) {
            doc.set(name(field), Value::Text(format!("{field}-value")));
        }
    }
    let mut counter = 0u8;
    for table in TABLES {
        if rng.chance(2) {
            continue;
        }
        let rows = 1 + rng.below(3);
        for _ in 0..rows {
            counter = counter.wrapping_add(1);
            let fields: BTreeMap<Name, Value> = ROW_FIELDS
                .iter()
                .filter(|_| rng.chance(2))
                .map(|field| (name(field), Value::Text(format!("{field}-value"))))
                .collect();
            doc.rows_mut(name(table)).push(ChildRow {
                id: row_id(counter),
                fields,
            });
        }
    }
    doc
}

fn random_policy(rng: &mut Lcg) -> SyncPolicy {
    // Excluded names are drawn from the parent, table **and** row-field pools: the rule is by
    // name, so a name is excluded wherever it appears.
    let mut excluded = BTreeSet::new();
    for candidate in PARENT_FIELDS
        .iter()
        .chain(TABLES.iter())
        .chain(ROW_FIELDS.iter())
    {
        if rng.chance(3) {
            excluded.insert(FieldName::new(candidate).unwrap());
        }
    }
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        excluded,
    )
}

#[test]
fn the_outgoing_view_is_always_accepted_for_the_same_policy() {
    for seed in 0..300u64 {
        let mut rng = Lcg(seed + 1);
        let origin = random_document(&mut rng);
        let policy = random_policy(&mut rng);
        let view = outgoing_view(&origin, &policy);

        assert!(
            accept_incoming("invoice", &view, &policy).is_ok(),
            "seed {seed}: the view of a document is always receivable; origin {origin:?}, \
             policy {policy:?}, view {view:?}"
        );

        // Nothing excluded survives at any level, and nothing else is lost.
        for (field, value) in origin.fields() {
            let kept = view.fields().get(field);
            if policy.excludes(field.as_str()) {
                assert!(kept.is_none(), "seed {seed}: parent field {field} leaked");
            } else {
                assert_eq!(
                    kept,
                    Some(value),
                    "seed {seed}: parent field {field} changed"
                );
            }
        }
        assert_eq!(
            view.fields().len(),
            origin
                .fields()
                .keys()
                .filter(|f| !policy.excludes(f.as_str()))
                .count(),
            "seed {seed}: no parent field is invented"
        );
        for (table, rows) in origin.tables() {
            if policy.excludes(table.as_str()) {
                assert!(
                    view.rows(table.as_str()).is_empty(),
                    "seed {seed}: excluded table {table} is absent"
                );
                continue;
            }
            let seen = view.rows(table.as_str());
            assert_eq!(
                seen.len(),
                rows.len(),
                "seed {seed}: table {table} keeps its row count"
            );
            for (kept, source) in seen.iter().zip(rows) {
                assert_eq!(kept.id, source.id, "seed {seed}: row ids and order hold");
                for (field, value) in &source.fields {
                    if policy.excludes(field.as_str()) {
                        assert!(
                            !kept.fields.contains_key(field),
                            "seed {seed}: row field {field} of {table} leaked"
                        );
                    } else {
                        assert_eq!(
                            kept.fields.get(field),
                            Some(value),
                            "seed {seed}: row field {field} of {table} changed"
                        );
                    }
                }
            }
        }
    }
}
