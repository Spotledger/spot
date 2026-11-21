//! Snapshot tests: the generated text of one DocType set is exactly its committed `.snap`
//! (regenerate a drifted expectation only with `cargo insta review` / `INSTA_UPDATE_ALWAYS`,
//! never by editing the snapshot to make a failure disappear).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_dts::{changed, generate, generate_for};
use spot_meta::{Definition, FieldSpec, FieldType};

/// A definition with no fields, named `name`.
fn def(name: &str) -> Definition {
    Definition::new(name)
}

/// A field named `name` of `ty`.
fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn field_ty_once(ty: FieldType) -> FieldSpec {
    let (name, opts) = match ty {
        FieldType::Bool => ("flag", None),
        FieldType::Int => ("count", None),
        FieldType::Decimal => ("amount", None),
        FieldType::Text => ("reference", None),
        FieldType::LongText => ("notes", None),
        FieldType::Select => ("stage", Some("open\nclosed\ncancelled")),
        FieldType::Date => ("posting_date", None),
        FieldType::Time => ("posting_time", None),
        FieldType::DateTime => ("created", None),
        FieldType::Link => ("party", Some("party")),
        FieldType::DynamicLink => ("target_doc", None),
        FieldType::Json => ("extra", None),
        FieldType::Bytes => ("payload", None),
        FieldType::Geo => ("position", None),
        FieldType::Range => ("validity", None),
        FieldType::Table => ("items", Some("sales_invoice_item")),
        FieldType::TableMultiselect => ("tags", Some("doc_note_tag")),
        FieldType::Attach => ("attachment", None),
        FieldType::Secret => ("password", None),
    };
    let mut f = FieldSpec::new(name, ty);
    f.options = opts.map(str::to_owned);
    f
}

/// One field of every kind, in one definition.
fn every_kind() -> Definition {
    let mut d = def("sales_invoice");
    for ty in [
        FieldType::Bool,
        FieldType::Int,
        FieldType::Decimal,
        FieldType::Text,
        FieldType::LongText,
        FieldType::Select,
        FieldType::Date,
        FieldType::Time,
        FieldType::DateTime,
        FieldType::Link,
        FieldType::DynamicLink,
        FieldType::Json,
        FieldType::Bytes,
        FieldType::Table,
        FieldType::TableMultiselect,
        FieldType::Attach,
        FieldType::Secret,
    ] {
        d.fields.push(field_ty_once(ty));
    }
    d
}

#[test]
fn snapshot_plans_the_whole_site() {
    let mut item = def("sales_invoice_item");
    item.is_child = true;
    item.fields = vec![field("item_name", FieldType::Text), {
        let mut q = FieldSpec::new("quantity", FieldType::Decimal);
        q.required = true;
        q.scale = Some(3);
        q
    }];
    let mut party = def("party");
    party.fields = vec![
        field("party_name", FieldType::Text),
        field("type", FieldType::Text),
        field("9lives", FieldType::Int),
    ];
    let mut invoice = every_kind();
    invoice.is_posted = true;
    // The child and the multi-pick target the table references.
    let mut item = def("sales_invoice_item");
    item.is_child = true;
    item.fields = vec![field("item_name", FieldType::Text), {
        let mut q = FieldSpec::new("quantity", FieldType::Decimal);
        q.required = true;
        q.scale = Some(3);
        q
    }];
    let mut party = def("party");
    party.fields = vec![
        field("party_name", FieldType::Text),
        field("type", FieldType::Text),
        field("9lives", FieldType::Int),
    ];
    let mut note = def("doc_note");
    note.label = Some("Note *with* specials */ here".to_owned());
    note.fields = vec![field("note_text", FieldType::LongText)];
    let mut tag = def("doc_note_tag");
    tag.is_child = true;
    tag.fields = vec![field("tag_name", FieldType::Text)];
    let generated = generate(&[invoice, item, party, note, tag]).unwrap();
    insta::assert_snapshot!(generated.text);
}

#[test]
fn snapshot_link_cycle_terminates_and_declares_both() {
    let mut a = def("cycle_a");
    a.fields = vec![field("other", FieldType::Link)];
    a.fields[0].options = Some("cycle_b".to_owned());
    let mut b = def("cycle_b");
    b.fields = vec![field("back", FieldType::Link)];
    b.fields[0].options = Some("cycle_a".to_owned());
    let generated = generate(&[a, b]).unwrap();
    insta::assert_snapshot!(generated.text);
}

#[test]
fn snapshot_custom_field_sits_next_to_standard_fields() {
    // A custom field is indistinguishable from a standard one here: spot-meta has already
    // merged it into the effective definition.
    let mut d = def("customer");
    d.fields = vec![field("customer_name", FieldType::Text)];
    let mut custom = FieldSpec::new("loyalty_tier", FieldType::Select);
    custom.options = Some("bronze\nsilver\ngold".to_owned());
    custom.label = Some("Loyalty tier (added by the client's app)".to_owned());
    d.fields.push(custom);
    let generated = generate(&[d]).unwrap();
    insta::assert_snapshot!(generated.text);
}

#[test]
fn snapshot_generate_for_is_smaller_and_closes_over_references() {
    let mut a = def("scoped_a");
    a.fields = vec![field("target", FieldType::Link)];
    a.fields[0].options = Some("scoped_b".to_owned());
    let b = def("scoped_b");
    let mut c = def("scoped_c");
    c.fields = vec![field("unrelated", FieldType::Text)];
    let generated = generate_for(&[a, b, c], &["scoped_a"]).unwrap();
    insta::assert_snapshot!(generated.text);
    assert!(generated.text.contains("interface ScopedB"));
    assert!(!generated.text.contains("interface ScopedC"));
}

#[test]
fn fingerprint_changes_when_a_field_is_added() {
    let mut d = def("fingerprinted");
    d.fields = vec![field("one", FieldType::Text)];
    let before = generate(&[d.clone()]).unwrap();
    d.fields.push(field("two", FieldType::Text));
    let after = generate(&[d]).unwrap();
    assert!(changed(&before.fingerprint, &after));
}

#[test]
fn fingerprint_stable_when_the_definitions_are_reordered() {
    let a = def("aa_alpha");
    let mut b = def("bb_beta");
    b.fields = vec![field("x", FieldType::Text)];
    let one = generate(&[a.clone(), b.clone()]).unwrap();
    let two = generate(&[b, a]).unwrap();
    assert!(!changed(&one.fingerprint, &two));
    assert_eq!(one.text, two.text);
}

#[test]
fn name_collision_names_both_sources() {
    let mut a = def("item_group");
    a.fields = vec![field("x", FieldType::Text)];
    // `item_group` and `item__group` both split into the words `item` + `group`, so both
    // PascalCase to `ItemGroup`; the error must name both sources, never silently rename.
    let b = def("item__group");
    let err = generate(&[a, b]);
    assert!(err.is_err(), "a collision must be an error");
    let message = err
        .unwrap_err()
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    assert!(
        message.contains("item_group"),
        "names both sources: {message}"
    );
    assert!(
        message.contains("item__group"),
        "names both sources: {message}"
    );
}

#[test]
fn unknown_link_target_is_an_error() {
    let mut d = def("dangling");
    d.fields = vec![field("who", FieldType::Link)];
    d.fields[0].options = Some("not_in_input".to_owned());
    let err = generate(&[d]).unwrap_err();
    assert!(err.iter().any(|e| e.to_string().contains("not_in_input")));
}

#[test]
fn decimal_is_string_in_the_generated_type() {
    let mut d = def("dec");
    d.fields = vec![field("amount", FieldType::Decimal)];
    let text = generate(&[d]).unwrap().text;
    assert!(text.contains("amount?: string;"), "{text}");
}
