//! Custom fields, property overrides, `insert_after` ordering and the metadata version.
#![allow(missing_docs)]

mod common;

use common::{fixtures, kinds, of, text};
use spot_app_api::meta::FieldKind;
use spot_app_api::meta::TextFormat;
use spot_meta::{
    Customisations, EffectiveMeta, FieldSpec, FieldType, MetaError, MetaErrorKind as K, Property,
    Registry,
};
use spot_storage_contract::Name;

fn build(c: &Customisations) -> Result<EffectiveMeta, Vec<MetaError>> {
    Registry::build(fixtures(), c)
}

fn field_names(m: &EffectiveMeta, doctype: &str) -> Vec<String> {
    m.definition(doctype)
        .map(|d| d.fields.iter().map(|f| f.name.clone()).collect())
        .unwrap_or_default()
}

fn refused(c: &Customisations, kind: K) -> MetaError {
    let e = build(c)
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [kind], "{e:?}");
    e.into_iter().next().unwrap_or_else(|| panic!("one error"))
}

#[test]
fn custom_fields_are_added_in_order() {
    let mut c = Customisations::new();
    c.add_field("invoice", text("custom_tail"), None);
    c.add_field("invoice", text("custom_after_number"), Some("number"));
    c.add_field(
        "invoice",
        text("custom_after_that"),
        Some("custom_after_number"),
    );
    c.add_field("invoice", text("custom_before_lines"), Some("status"));
    let m = build(&c).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        field_names(&m, "invoice"),
        [
            "number",
            "custom_after_number",
            "custom_after_that",
            "party",
            "posting_date",
            "status",
            "custom_before_lines",
            "lines",
            "total",
            "remarks",
            "custom_tail",
            "doc_state",
            "amended_from",
        ]
    );
    // The same order reaches the app view.
    let view = m.doctype("invoice").unwrap_or_else(|| panic!("invoice"));
    let names: Vec<_> = view.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names[1], "custom_after_number");
    assert_eq!(names[names.len() - 3], "custom_tail");
}

#[test]
fn custom_fields_reach_the_catalog() {
    let mut c = Customisations::new();
    let mut f = text("custom_ref");
    f.unique = true;
    c.add_field("party", f, None);
    c.add_field(
        "party",
        of(FieldType::Link, "custom_parent", Some("party")),
        None,
    );
    let m = build(&c).unwrap_or_else(|e| panic!("{e:?}"));
    let cat = m
        .catalog(common_collation())
        .unwrap_or_else(|e| panic!("{e}"));
    let party = cat.entity(&name("party")).unwrap_or_else(|e| panic!("{e}"));
    assert!(party.field(&name("custom_ref")).is_ok());
    assert!(
        party
            .indexes
            .iter()
            .any(|i| i.name == name("uq_party_custom_ref") && i.unique)
    );
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn common_collation() -> spot_storage_contract::schema::Collation {
    spot_storage_contract::schema::Collation::new("en").unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn custom_field_rules() {
    let mut c = Customisations::new();
    c.add_field("invoice", text("remarks2"), None);
    refused(&c, K::BadCustomField);

    let mut c = Customisations::new();
    c.add_field("invoice", text("custom_x"), Some("nope"));
    refused(&c, K::UnknownName);

    let mut c = Customisations::new();
    c.add_field("nowhere", text("custom_x"), None);
    refused(&c, K::UnknownName);

    let mut c = Customisations::new();
    c.add_field("invoice", text("custom_x"), None);
    c.add_field("invoice", text("custom_x"), None);
    refused(&c, K::Duplicate);

    // Custom fields get the same validation as standard ones.
    let mut c = Customisations::new();
    c.add_field(
        "invoice",
        of(FieldType::Link, "custom_l", Some("ghost")),
        None,
    );
    refused(&c, K::UnknownTarget);
    let mut c = Customisations::new();
    c.add_field(
        "invoice",
        FieldSpec::new("custom_d", FieldType::Decimal),
        None,
    );
    refused(&c, K::BadProperty);
}

#[test]
fn accepted_overrides() {
    let mut c = Customisations::new();
    c.set("invoice", None, Property::Label("Sales Document".into()));
    c.set("invoice", None, Property::TitleField("remarks".into()));
    c.set("invoice", Some("number"), Property::Label("No.".into()));
    c.set("invoice", Some("number"), Property::Required(true));
    c.set("invoice", Some("number"), Property::MaxChars(200));
    c.set("invoice", Some("total"), Property::ReadOnly(false));
    c.set("invoice", Some("total"), Property::AllowOnPosted(true));
    c.set(
        "invoice",
        Some("status"),
        Property::Default(Some("Paid".into())),
    );
    c.set(
        "invoice",
        Some("status"),
        Property::Options(vec![
            "Open".into(),
            "Paid".into(),
            "Overdue".into(),
            "Disputed".into(),
        ]),
    );
    let m = build(&c).unwrap_or_else(|e| panic!("{e:?}"));
    let d = m.definition("invoice").unwrap_or_else(|| panic!("invoice"));
    assert_eq!(d.label.as_deref(), Some("Sales Document"));
    assert_eq!(d.title_field.as_deref(), Some("remarks"));
    let f = |n: &str| {
        d.fields
            .iter()
            .find(|f| f.name == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert_eq!(f("number").label.as_deref(), Some("No."));
    assert!(f("number").required);
    assert_eq!(f("number").max_chars, Some(200));
    assert!(!f("total").read_only && f("total").allow_on_posted);
    assert_eq!(f("status").default.as_deref(), Some("Paid"));
    assert_eq!(
        f("status").select_values(),
        ["Open", "Paid", "Overdue", "Disputed"]
    );

    let view = m.doctype("invoice").unwrap_or_else(|| panic!("invoice"));
    assert_eq!(view.label, "Sales Document");
    assert_eq!(
        view.field("number").map(|f| f.kind.clone()),
        Some(FieldKind::Text {
            max_chars: Some(200),
            format: TextFormat::Plain
        })
    );
    // ADR-068 §6: the override's default and the select values reach the app view.
    assert!(view.field("total").is_some_and(|f| f.allow_on_posted));
    assert_eq!(
        view.field("status").and_then(|f| f.default.clone()),
        Some(spot_storage_contract::Value::Text("Paid".into()))
    );
    assert_eq!(
        view.field("status").map(|f| f.kind.clone()),
        Some(FieldKind::Select {
            values: ["Open", "Paid", "Overdue", "Disputed"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
    );
}

#[test]
fn a_later_override_wins() {
    let mut c = Customisations::new();
    c.set("party", Some("code"), Property::Label("One".into()));
    c.set("party", Some("code"), Property::Label("Two".into()));
    let m = build(&c).unwrap_or_else(|e| panic!("{e:?}"));
    let label = m
        .definition("party")
        .and_then(|d| d.fields[0].label.clone());
    assert_eq!(label.as_deref(), Some("Two"));
}

#[test]
fn refused_overrides() {
    // Options can only be added to.
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("status"),
        Property::Options(vec!["Open".into(), "Paid".into()]),
    );
    assert!(refused(&c, K::BadOverride).message.contains("Overdue"));

    // max_chars can only be raised, and only where a limit exists.
    let mut c = Customisations::new();
    c.set("invoice", Some("number"), Property::MaxChars(10));
    assert!(refused(&c, K::BadOverride).message.contains("raised"));
    let mut c = Customisations::new();
    c.set("party", Some("code"), Property::MaxChars(10));
    refused(&c, K::BadOverride);
    let mut c = Customisations::new();
    c.set("party", Some("active"), Property::MaxChars(10));
    refused(&c, K::BadOverride);

    // Options only on select fields; title_field only on a DocType; label etc. the other way.
    let mut c = Customisations::new();
    c.set("party", Some("code"), Property::Options(vec!["a".into()]));
    refused(&c, K::BadOverride);
    let mut c = Customisations::new();
    c.set("party", Some("code"), Property::TitleField("code".into()));
    refused(&c, K::BadOverride);
    let mut c = Customisations::new();
    c.set("party", None, Property::Required(true));
    refused(&c, K::BadOverride);

    // Unknown DocType or field.
    let mut c = Customisations::new();
    c.set("ghost", None, Property::Label("x".into()));
    refused(&c, K::UnknownName);
    let mut c = Customisations::new();
    c.set("party", Some("ghost"), Property::Required(true));
    refused(&c, K::UnknownName);

    // The result is still validated: a bad title field or default is caught.
    let mut c = Customisations::new();
    c.set("invoice", None, Property::TitleField("total".into()));
    refused(&c, K::BadTitleField);
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("status"),
        Property::Default(Some("Nope".into())),
    );
    refused(&c, K::BadDefault);

    // Engine fields cannot be overridden.
    let mut c = Customisations::new();
    c.set("invoice", Some("doc_state"), Property::Required(true));
    refused(&c, K::UnknownName);
}

#[test]
fn types_and_names_cannot_be_overridden() {
    for key in ["type", "name", "scale", "precision", "unique", "index"] {
        let e = Property::parse(key, "x")
            .err()
            .unwrap_or_else(|| panic!("{key} must be refused"));
        assert_eq!(e.kind, K::BadOverride);
        assert!(e.message.contains(key), "{}", e.message);
    }
    let e = Property::parse("colour", "red")
        .err()
        .unwrap_or_else(|| panic!("refused"));
    assert!(e.message.contains("colour"));
}

#[test]
fn property_text_forms_parse() {
    let ok = |k, v| Property::parse(k, v).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ok("required", "true"), Property::Required(true));
    assert_eq!(ok("read_only", "false"), Property::ReadOnly(false));
    assert_eq!(ok("allow_on_posted", "true"), Property::AllowOnPosted(true));
    assert_eq!(ok("max_chars", "40"), Property::MaxChars(40));
    assert_eq!(ok("default", ""), Property::Default(None));
    assert_eq!(ok("default", "x"), Property::Default(Some("x".into())));
    assert_eq!(
        ok("options", "a\n b \n\nc"),
        Property::Options(vec!["a".into(), "b".into(), "c".into()])
    );
    assert!(Property::parse("required", "yes").is_err());
    assert!(Property::parse("max_chars", "-1").is_err());
}

#[test]
fn the_version_rises_on_every_change() {
    let mut reg = Registry::new(fixtures()).unwrap_or_else(|e| panic!("{e:?}"));
    let v0 = reg.effective().version();
    let v1 = reg
        .add_custom_field("party", text("custom_a"), None)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let v2 = reg
        .set_property("party", Some("code"), Property::Label("C".into()))
        .unwrap_or_else(|e| panic!("{e:?}"));
    let v3 = reg
        .remove_custom_field("party", "custom_a")
        .unwrap_or_else(|e| panic!("{e:?}"));
    let v4 = reg
        .set_definitions(fixtures())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        v0 < v1 && v1 < v2 && v2 < v3 && v3 < v4,
        "{v0} {v1} {v2} {v3} {v4}"
    );
    assert_eq!(reg.effective().version(), v4);

    // The catalog and every DocTypeDef carry the version.
    let m = reg.effective();
    let cat = m
        .catalog(common_collation())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cat.version, v4);
    assert!(m.doctypes().all(|d| d.version == v4));
}

#[test]
fn a_refused_change_changes_nothing() {
    let mut reg = Registry::new(fixtures()).unwrap_or_else(|e| panic!("{e:?}"));
    let before = reg.effective();
    assert!(
        reg.add_custom_field("party", text("not_custom"), None)
            .is_err()
    );
    assert!(
        reg.set_property("party", Some("code"), Property::Options(vec![]))
            .is_err()
    );
    assert!(reg.remove_custom_field("party", "custom_missing").is_err());
    let after = reg.effective();
    assert_eq!(before.version(), after.version());
    assert_eq!(reg.customisations().revision(), 0);
}

#[test]
fn removing_a_custom_field_drops_its_overrides() {
    let mut reg = Registry::new(fixtures()).unwrap_or_else(|e| panic!("{e:?}"));
    reg.add_custom_field("party", text("custom_a"), None)
        .unwrap_or_else(|e| panic!("{e:?}"));
    reg.set_property("party", Some("custom_a"), Property::Required(true))
        .unwrap_or_else(|e| panic!("{e:?}"));
    reg.remove_custom_field("party", "custom_a")
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        reg.effective()
            .definition("party")
            .is_some_and(|d| d.fields.iter().all(|f| f.name != "custom_a"))
    );
    assert!(reg.customisations().overrides().is_empty());
}

#[test]
fn an_old_snapshot_is_not_affected_by_a_later_change() {
    let mut reg = Registry::new(fixtures()).unwrap_or_else(|e| panic!("{e:?}"));
    let old = reg.effective();
    reg.add_custom_field("party", text("custom_a"), None)
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        old.definition("party")
            .is_some_and(|d| d.fields.iter().all(|f| f.name != "custom_a"))
    );
    assert!(
        reg.effective()
            .definition("party")
            .is_some_and(|d| d.fields.iter().any(|f| f.name == "custom_a"))
    );
}
