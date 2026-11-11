//! Property descriptors (ADR-068 §1): the table is complete, drives the override rules, and the
//! presentation hints travel from a definition to the app view.
#![allow(missing_docs)]

mod common;

use common::{fixtures, kinds};
use spot_meta::descriptor::{
    Layer, Overridable, Target, descriptor, descriptors, doctype_descriptor,
};
use spot_meta::{
    Customisations, Definition, FieldLookup, FieldSpec, FieldType, IndexSpec, LookupSpec, MaskSpec,
    MetaErrorKind as K, Property, ProtectionGroupSpec, Registry,
};

fn keys<T: serde::Serialize>(value: &T) -> Vec<String> {
    match toml::Value::try_from(value) {
        Ok(toml::Value::Table(t)) => t.keys().cloned().collect(),
        other => panic!("not a table: {other:?}"),
    }
}

/// A field with every property set, so every key is serialised.
fn full_field() -> FieldSpec {
    let mut f = FieldSpec::new("f", FieldType::Text);
    f.label = Some("F".into());
    f.options = Some("o".into());
    f.required = true;
    f.read_only = true;
    f.allow_on_posted = true;
    f.unique = true;
    f.index = true;
    f.multiple = true;
    f.default = Some("d".into());
    f.depends_on = Some("doc.a".into());
    f.mandatory_depends_on = Some("doc.b".into());
    f.read_only_depends_on = Some("doc.c".into());
    f.fetch_from = Some("l.n".into());
    f.scale = Some(2);
    f.precision = Some(10);
    f.max_chars = Some(5);
    f.format = Some("plain".into());
    f.syntax = Some("sql".into());
    f.symbology = Some("ean13".into());
    f.max = Some(5);
    f.currency_field = Some("c".into());
    f.description = Some("help".into());
    f.hidden = true;
    f.in_list_view = true;
    f.in_standard_filter = true;
    f.no_copy = true;
    f.control = Some("dropdown".into());
    f.permlevel = 1;
    f.lookup = Some(FieldLookup::Spec(LookupSpec::seam(
        "sample_app:thing_search",
    )));
    f.protection_group = Some("pay".into());
    f.classification = vec!["pii".into()];
    f.masking = Some(MaskSpec::placeholder());
    f.declassified = Some("review required".into());
    f
}

fn full_definition() -> Definition {
    let mut d = Definition::new("d");
    d.label = Some("D".into());
    d.app = Some("a".into());
    d.module = Some("m".into());
    d.is_posted = true;
    d.is_child = true;
    d.title_field = Some("f".into());
    d.naming = Some("hash".into());
    // Add full_field with protection_group, and a second field with permlevel
    // (they are mutually exclusive, so we need both fields to test all properties).
    let mut f2 = FieldSpec::new("g", FieldType::Text);
    f2.permlevel = 2;
    d.fields = vec![full_field(), f2];
    d.indexes = vec![IndexSpec {
        fields: vec!["f".into()],
        unique: true,
    }];
    d.protection_groups = vec![ProtectionGroupSpec::new("group", "Group")];
    d
}

#[test]
fn every_serialised_property_has_a_descriptor() {
    for key in keys(&full_field()) {
        assert!(
            descriptor(&key).is_some(),
            "field property `{key}` has no descriptor"
        );
    }
    for key in keys(&full_definition()) {
        assert!(
            doctype_descriptor(&key).is_some(),
            "DocType property `{key}` has no descriptor"
        );
    }
}

#[test]
fn every_live_descriptor_is_a_real_property() {
    let field_keys = keys(&full_field());
    let doctype_keys = keys(&full_definition());
    for d in descriptors().iter().filter(|d| d.pending.is_none()) {
        let known = match d.target {
            Target::Field => &field_keys,
            Target::DocType => &doctype_keys,
        };
        assert!(
            known.iter().any(|k| k == d.name),
            "descriptor `{}` names no property",
            d.name
        );
    }
}

#[test]
fn descriptor_names_are_unique_per_target() {
    for (i, a) in descriptors().iter().enumerate() {
        for b in &descriptors()[i + 1..] {
            assert!(
                !(a.name == b.name && a.target == b.target),
                "`{}` is described twice",
                a.name
            );
        }
    }
}

#[test]
fn layers_follow_the_adr() {
    let layer = |n: &str| descriptor(n).map(|d| d.layer);
    for n in [
        "name",
        "type",
        "format",
        "scale",
        "precision",
        "max_chars",
        "unique",
        "index",
    ] {
        assert_eq!(layer(n), Some(Layer::Storage), "{n}");
    }
    for n in [
        "required",
        "read_only",
        "allow_on_posted",
        "default",
        "options",
        "depends_on",
        "mandatory_depends_on",
        "read_only_depends_on",
        "fetch_from",
    ] {
        assert_eq!(layer(n), Some(Layer::Rules), "{n}");
    }
    assert_eq!(layer("lookup"), Some(Layer::Behaviour));
    for n in [
        "label",
        "description",
        "hidden",
        "in_list_view",
        "in_standard_filter",
        "no_copy",
        "permlevel",
    ] {
        assert_eq!(layer(n), Some(Layer::Presentation), "{n}");
    }
}

#[test]
fn expression_properties_are_real_site_properties() {
    for n in [
        "depends_on",
        "mandatory_depends_on",
        "read_only_depends_on",
        "fetch_from",
    ] {
        let d = descriptor(n).unwrap_or_else(|| panic!("{n} missing"));
        assert_eq!(d.pending, None);
        assert_eq!(d.layer, Layer::Rules);
        assert_eq!(d.overridable, Overridable::Site);
        assert_eq!(Property::parse(n, "a.b").map(|p| p.key()), Ok(n));
        let text =
            format!("name = \"d\"\n[[fields]]\nname = \"f\"\ntype = \"text\"\n{n} = \"a.b\"\n");
        assert!(Definition::parse("d.toml", &text).is_ok(), "{n}");
    }
    // Empty text clears an override.
    assert_eq!(
        Property::parse("depends_on", ""),
        Ok(Property::DependsOn(None))
    );
}

#[test]
fn naming_is_a_storage_doctype_property_no_site_may_change() {
    let d = doctype_descriptor("naming").unwrap_or_else(|| panic!("naming"));
    assert_eq!(d.layer, Layer::Storage);
    assert_eq!(d.overridable, Overridable::Never);
    let e = Property::parse("naming", "hash")
        .err()
        .unwrap_or_else(|| panic!("naming must be refused"));
    assert_eq!(e.kind, K::BadOverride);
}

#[test]
fn never_properties_are_refused_with_the_layer() {
    let never: Vec<_> = descriptors()
        .iter()
        .filter(|d| d.overridable == Overridable::Never && d.pending.is_none())
        .collect();
    assert!(never.len() >= 8);
    for d in never {
        let e = Property::parse(d.name, "x")
            .err()
            .unwrap_or_else(|| panic!("{} must be refused", d.name));
        assert_eq!(e.kind, K::BadOverride);
        assert!(e.message.contains(d.name), "{}", e.message);
        assert!(e.message.contains(d.layer.name()), "{}", e.message);
    }
}

/// A text value that parses for each overridable property.
fn sample(key: &str) -> &'static str {
    match key {
        "required" | "read_only" | "allow_on_posted" | "hidden" | "in_list_view"
        | "in_standard_filter" | "no_copy" => "true",
        "max_chars" | "permlevel" => "7",
        "lookup" => "seam = \"sample_app:thing_search\"",
        "protection_group" => "group",
        "classification" => "pii",
        "masking" => "placeholder = true",
        "control" => "dropdown",
        _ => "x",
    }
}

#[test]
fn site_properties_are_accepted_and_round_trip_their_key() {
    let site: Vec<_> = descriptors()
        .iter()
        .filter(|d| d.overridable != Overridable::Never && d.pending.is_none())
        .collect();
    assert!(site.len() >= 14);
    for d in site {
        let p =
            Property::parse(d.name, sample(d.name)).unwrap_or_else(|e| panic!("{}: {e}", d.name));
        assert_eq!(p.key(), d.name);
    }
}

#[test]
fn a_view_may_not_override_the_permission_level() {
    let d = descriptor("permlevel").unwrap_or_else(|| panic!("permlevel"));
    assert_eq!(d.overridable, Overridable::Site);
    for n in ["label", "hidden", "in_list_view", "description"] {
        let d = descriptor(n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(d.overridable, Overridable::SiteAndView);
    }
}

#[test]
fn applies_to_decides_where_a_property_is_valid() {
    let a = |n: &str| descriptor(n).unwrap_or_else(|| panic!("{n}")).applies_to;
    assert!(a("scale").accepts(FieldType::Decimal, None));
    assert!(!a("scale").accepts(FieldType::Int, None));
    assert!(a("label").accepts(FieldType::Json, None));
    assert!(a("syntax").accepts(FieldType::LongText, Some("code")));
    assert!(!a("syntax").accepts(FieldType::LongText, Some("markdown")));
    assert!(!a("syntax").accepts(FieldType::LongText, None));
    assert!(a("currency_field").accepts(FieldType::Decimal, Some("currency")));
    assert!(!a("default").accepts(FieldType::Secret, None));
    assert!(a("max_chars").describe().contains("text"));
}

fn refused(c: &Customisations) -> spot_meta::MetaError {
    let e = Registry::build(fixtures(), c)
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadOverride], "{e:?}");
    e.into_iter().next().unwrap_or_else(|| panic!("one"))
}

#[test]
fn a_property_on_a_type_it_does_not_apply_to_is_refused() {
    // Options belong to select, link and table types; `active` is a bool.
    let mut c = Customisations::new();
    c.set("party", Some("active"), Property::Options(vec!["a".into()]));
    let e = refused(&c);
    assert!(e.message.contains("options"), "{}", e.message);
    // A default on a table field.
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("lines"),
        Property::Default(Some("x".into())),
    );
    refused(&c);
    // max_chars on a decimal.
    let mut c = Customisations::new();
    c.set("invoice", Some("total"), Property::MaxChars(10));
    refused(&c);
}

#[test]
fn doctype_properties_follow_their_descriptors() {
    let mut c = Customisations::new();
    c.set("party", None, Property::Hidden(true));
    refused(&c);
    let mut c = Customisations::new();
    c.set("party", Some("code"), Property::TitleField("code".into()));
    refused(&c);
    let mut c = Customisations::new();
    c.set("party", None, Property::Label("Customer".into()));
    c.set("party", None, Property::TitleField("code".into()));
    assert!(Registry::build(fixtures(), &c).is_ok());
}

#[test]
fn hints_are_overridden_and_reach_the_app_view() {
    let mut c = Customisations::new();
    for p in [
        Property::Description(Some("The party code".into())),
        Property::Hidden(true),
        Property::InListView(true),
        Property::InStandardFilter(true),
        Property::NoCopy(true),
        Property::Permlevel(2),
    ] {
        c.set("party", Some("code"), p);
    }
    let m = Registry::build(fixtures(), &c).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("party").unwrap_or_else(|| panic!("party"));
    let h = &view.field("code").unwrap_or_else(|| panic!("code")).hints;
    assert_eq!(h.description.as_deref(), Some("The party code"));
    assert!(h.hidden && h.in_list_view && h.in_standard_filter && h.no_copy);
    // permlevel is always 0 in compiled output (ADR-068 A1: deprecated).
    assert_eq!(h.permlevel, 0);
    // Another field keeps the defaults.
    let other = &view
        .field("active")
        .unwrap_or_else(|| panic!("active"))
        .hints;
    assert_eq!(other.permlevel, 0);
    assert!(!other.hidden && other.description.is_none());

    // A later override clears the description.
    c.set("party", Some("code"), Property::Description(None));
    let m = Registry::build(fixtures(), &c).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("party").unwrap_or_else(|| panic!("party"));
    assert_eq!(
        view.field("code").and_then(|f| f.hints.description.clone()),
        None
    );
}

#[test]
fn hints_round_trip_through_toml_and_reach_the_app_view() {
    let mut d = Definition::new("note");
    let mut f = FieldSpec::new("body", FieldType::LongText);
    f.description = Some("What happened".into());
    f.hidden = true;
    f.in_list_view = true;
    f.in_standard_filter = true;
    f.no_copy = true;
    f.permlevel = 3;
    d.fields = vec![f.clone(), FieldSpec::new("plain", FieldType::Text)];
    let text = d.to_toml().unwrap_or_else(|e| panic!("{e}"));
    // Defaults are not written.
    assert_eq!(text.matches("permlevel").count(), 1, "{text}");
    assert_eq!(text.matches("hidden").count(), 1, "{text}");
    let back = Definition::parse("note.toml", &text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, d);

    let m = common::build(vec![back]).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("note").unwrap_or_else(|| panic!("note"));
    let h = &view.field("body").unwrap_or_else(|| panic!("body")).hints;
    assert_eq!(h.description.as_deref(), Some("What happened"));
    assert!(h.hidden && h.in_list_view && h.in_standard_filter && h.no_copy);
    // permlevel is always 0 in compiled output (ADR-068 A1: deprecated).
    assert_eq!(h.permlevel, 0);
}

#[test]
fn hint_text_forms_parse() {
    let ok = |k, v| Property::parse(k, v).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ok("hidden", "true"), Property::Hidden(true));
    assert_eq!(ok("permlevel", "4"), Property::Permlevel(4));
    assert_eq!(ok("description", ""), Property::Description(None));
    assert_eq!(
        ok("description", "x"),
        Property::Description(Some("x".into()))
    );
    assert!(Property::parse("permlevel", "256").is_err());
    assert!(Property::parse("no_copy", "yes").is_err());
}
