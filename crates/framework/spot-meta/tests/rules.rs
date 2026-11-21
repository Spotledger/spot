//! The rule properties (ADR-068 §1, WP-MET-01.T7): `depends_on`, `mandatory_depends_on`,
//! `read_only_depends_on`, `fetch_from`, expression defaults and `naming`. They are text:
//! stored, shape-checked, overridden and published, never parsed.
#![allow(missing_docs)]

mod common;

use common::{build, kinds, of, simple, text};
use spot_meta::{
    Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, MetaErrorKind as K, Property,
    Registry,
};

fn party() -> Definition {
    simple("party", vec![text("code"), text("tax_id")])
}

/// `doc` has a link to `party`, a text and an int.
fn doc(extra: Vec<FieldSpec>) -> Vec<Definition> {
    let mut fields = vec![
        of(FieldType::Link, "party", Some("party")),
        text("tax"),
        FieldSpec::new("qty", FieldType::Int),
    ];
    fields.extend(extra);
    vec![party(), simple("doc", fields)]
}

fn field_mut<'a>(defs: &'a mut [Definition], name: &str) -> &'a mut FieldSpec {
    defs.iter_mut()
        .find(|d| d.name == "doc")
        .and_then(|d| d.fields.iter_mut().find(|f| f.name == name))
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn ok(defs: Vec<Definition>) -> EffectiveMeta {
    build(defs).unwrap_or_else(|e| panic!("{e:?}"))
}

fn err(defs: Vec<Definition>) -> Vec<spot_meta::MetaError> {
    build(defs)
        .err()
        .unwrap_or_else(|| panic!("expected errors"))
}

#[test]
fn each_expression_property_round_trips_through_toml_and_the_app_view() {
    let mut defs = doc(vec![]);
    let f = field_mut(&mut defs, "qty");
    f.depends_on = Some("doc.tax != \"\"".into());
    f.mandatory_depends_on = Some("doc.party".into());
    f.read_only_depends_on = Some("doc.qty > 100".into());
    f.default = Some("=doc.qty * 2".into());
    let t = field_mut(&mut defs, "tax");
    t.fetch_from = Some("party.tax_id".into());
    defs[1].naming = Some("INV-.YYYY.-.#####".into());
    let toml = defs[1].to_toml().unwrap_or_else(|e| panic!("{e}"));
    let back = Definition::parse("doc.toml", &toml).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, defs[1]);

    let m = ok(defs);
    let r = m
        .field_rules("doc", "qty")
        .unwrap_or_else(|| panic!("rules"));
    assert_eq!(r.depends_on.as_deref(), Some("doc.tax != \"\""));
    assert_eq!(r.mandatory_depends_on.as_deref(), Some("doc.party"));
    assert_eq!(r.read_only_depends_on.as_deref(), Some("doc.qty > 100"));
    assert_eq!(r.default_expression.as_deref(), Some("doc.qty * 2"));
    assert!(r.default_is_expression());
    let t = m
        .field_rules("doc", "tax")
        .unwrap_or_else(|| panic!("rules"));
    assert_eq!(t.fetch_from.as_deref(), Some("party.tax_id"));
    assert!(!t.default_is_expression());
    assert_eq!(m.naming("doc"), Some("INV-.YYYY.-.#####"));
    assert_eq!(m.naming("party"), None);
    assert!(m.field_rules("doc", "nope").is_none());
}

#[test]
fn an_expression_default_is_kept_as_text_not_parsed_by_type() {
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "qty").default = Some("=doc.qty * 2".into());
    let m = ok(defs);
    let view = m.doctype("doc").unwrap_or_else(|| panic!("view"));
    let qty = view
        .fields
        .iter()
        .find(|f| f.name.as_str() == "qty")
        .unwrap_or_else(|| panic!("qty"));
    // An expression is not a value the app view can hold: it is the plan's job.
    assert_eq!(qty.default, None);
}

#[test]
fn a_double_equals_default_is_the_literal_with_one_equals() {
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "tax").default = Some("==x".into());
    let m = ok(defs);
    let r = m
        .field_rules("doc", "tax")
        .unwrap_or_else(|| panic!("rules"));
    assert!(!r.default_is_expression());
    let view = m.doctype("doc").unwrap_or_else(|| panic!("view"));
    let tax = view
        .fields
        .iter()
        .find(|f| f.name.as_str() == "tax")
        .unwrap_or_else(|| panic!("tax"));
    assert_eq!(
        tax.default,
        Some(spot_app_api::prelude::Value::Text("=x".into()))
    );
    // The literal is checked by type: `==5` on an int is the text `=5`, not an int.
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "qty").default = Some("==5".into());
    assert_eq!(kinds(&err(defs)), [K::BadDefault]);
}

#[test]
fn an_unprefixed_default_behaves_as_before() {
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "qty").default = Some("7".into());
    field_mut(&mut defs, "tax").default = Some("abc".into());
    let m = ok(defs);
    assert!(
        !m.field_rules("doc", "qty")
            .is_some_and(|r| r.default_is_expression())
    );
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "qty").default = Some("seven".into());
    assert_eq!(kinds(&err(defs)), [K::BadDefault]);
}

#[test]
fn expression_defaults_are_shape_checked_and_refused_where_no_default_exists() {
    for bad in ["=", "=   ", &format!("={}", "a".repeat(1001))] {
        let mut defs = doc(vec![]);
        field_mut(&mut defs, "qty").default = Some(bad.to_owned());
        assert_eq!(kinds(&err(defs)), [K::BadDefault], "{bad:.20}");
    }
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "party").default = Some("=doc.tax".into());
    assert_eq!(kinds(&err(defs)), [K::BadDefault]);
    let mut defs = doc(vec![FieldSpec::new("pw", FieldType::Secret)]);
    field_mut(&mut defs, "pw").default = Some("=doc.tax".into());
    assert_eq!(kinds(&err(defs)), [K::BadDefault]);
}

#[test]
fn depends_on_properties_refuse_empty_blank_and_long_text_naming_the_field() {
    for key in ["depends_on", "mandatory_depends_on", "read_only_depends_on"] {
        for bad in ["", "  \t", &"a".repeat(1001)] {
            let mut defs = doc(vec![]);
            let f = field_mut(&mut defs, "qty");
            match key {
                "depends_on" => f.depends_on = Some(bad.into()),
                "mandatory_depends_on" => f.mandatory_depends_on = Some(bad.into()),
                _ => f.read_only_depends_on = Some(bad.into()),
            }
            let e = err(defs);
            assert_eq!(kinds(&e), [K::BadProperty], "{key}");
            assert_eq!(e[0].doctype.as_deref(), Some("doc"));
            assert_eq!(e[0].field.as_deref(), Some("qty"));
            assert!(e[0].message.contains(key), "{}", e[0].message);
        }
    }
    // The cap is inclusive.
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "qty").depends_on = Some("a".repeat(1000));
    ok(defs);
}

#[test]
fn depends_on_properties_are_valid_on_every_field_type() {
    let mut defs = doc(vec![
        of(FieldType::Select, "kind", Some("a\nb")),
        FieldSpec::new("on", FieldType::Bool),
        FieldSpec::new("blob", FieldType::Json),
    ]);
    for name in ["party", "tax", "qty", "kind", "on", "blob"] {
        let f = field_mut(&mut defs, name);
        f.depends_on = Some("doc.qty > 1".into());
        f.mandatory_depends_on = Some("doc.qty > 2".into());
        f.read_only_depends_on = Some("doc.qty > 3".into());
    }
    ok(defs);
}

#[test]
fn fetch_from_must_be_a_pair_over_a_link_field_of_the_same_doctype() {
    for bad in [
        "partytax_id",
        "party.",
        ".tax_id",
        "party.tax.id",
        "Party.tax_id",
        "party.Tax",
    ] {
        let mut defs = doc(vec![]);
        field_mut(&mut defs, "tax").fetch_from = Some(bad.into());
        let e = err(defs);
        assert_eq!(kinds(&e), [K::BadProperty], "{bad}");
        assert_eq!(e[0].field.as_deref(), Some("tax"));
        assert!(e[0].message.contains("fetch_from"), "{}", e[0].message);
    }
    // The link field is absent.
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "tax").fetch_from = Some("nowhere.tax_id".into());
    assert_eq!(kinds(&err(defs)), [K::BadProperty]);
    // The link field is not a link.
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "tax").fetch_from = Some("qty.tax_id".into());
    assert_eq!(kinds(&err(defs)), [K::BadProperty]);
    // The target field of another DocType is not looked at here.
    let mut defs = doc(vec![]);
    field_mut(&mut defs, "tax").fetch_from = Some("party.no_such_field".into());
    ok(defs);
}

#[test]
fn fetch_from_is_refused_on_links_tables_and_secrets() {
    let mut defs = doc(vec![
        of(FieldType::Link, "other", Some("party")),
        FieldSpec::new("pw", FieldType::Secret),
    ]);
    for name in ["other", "pw"] {
        field_mut(&mut defs, name).fetch_from = Some("party.tax_id".into());
    }
    let e = err(defs);
    assert_eq!(kinds(&e), [K::BadProperty, K::BadProperty]);

    let mut child = simple("line", vec![text("note")]);
    child.is_child = true;
    let mut parent = simple(
        "order",
        vec![
            of(FieldType::Link, "party", Some("party")),
            of(FieldType::Table, "lines", Some("line")),
        ],
    );
    parent.fields[1].fetch_from = Some("party.tax_id".into());
    let e = err(vec![party(), child, parent]);
    assert_eq!(kinds(&e), [K::BadProperty]);
    assert_eq!(e[0].field.as_deref(), Some("lines"));
}

#[test]
fn naming_must_be_non_empty_and_within_200_bytes() {
    for bad in ["", "   "] {
        let mut defs = doc(vec![]);
        defs[1].naming = Some(bad.into());
        let e = err(defs);
        assert_eq!(kinds(&e), [K::BadProperty]);
        assert_eq!(e[0].doctype.as_deref(), Some("doc"));
        assert!(e[0].field.is_none());
    }
    let mut defs = doc(vec![]);
    defs[1].naming = Some("a".repeat(201));
    assert_eq!(kinds(&err(defs)), [K::BadProperty]);
    for good in [
        "a".repeat(200),
        "hash".into(),
        "prompt".into(),
        "field:tax".into(),
    ] {
        let mut defs = doc(vec![]);
        defs[1].naming = Some(good);
        ok(defs);
    }
}

#[test]
fn an_unknown_key_is_still_a_parse_error() {
    let t = "name = \"d\"\nnamings = \"hash\"\n";
    assert!(Definition::parse("d.toml", t).is_err());
    let t = "name = \"d\"\n[[fields]]\nname = \"f\"\ntype = \"text\"\nfetch_form = \"a.b\"\n";
    assert!(Definition::parse("d.toml", t).is_err());
}

fn built(c: &Customisations) -> Result<EffectiveMeta, Vec<spot_meta::MetaError>> {
    Registry::build(doc(vec![]), c)
}

#[test]
fn each_expression_override_is_set_replaced_and_cleared_and_the_version_rises() {
    type Make = fn(Option<String>) -> Property;
    let makers: [(Make, &str); 3] = [
        (Property::DependsOn, "depends_on"),
        (Property::MandatoryDependsOn, "mandatory_depends_on"),
        (Property::ReadOnlyDependsOn, "read_only_depends_on"),
    ];
    for (make, key) in makers {
        let read = |m: &EffectiveMeta| {
            let r = m
                .field_rules("doc", "qty")
                .unwrap_or_else(|| panic!("rules"));
            match key {
                "depends_on" => r.depends_on,
                "mandatory_depends_on" => r.mandatory_depends_on,
                _ => r.read_only_depends_on,
            }
        };
        let mut c = Customisations::new();
        let v0 = built(&c).unwrap_or_else(|e| panic!("{e:?}")).version();
        c.set("doc", Some("qty"), make(Some("doc.tax".into())));
        let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(read(&m).as_deref(), Some("doc.tax"));
        assert!(m.version() > v0);
        let v1 = m.version();
        c.set("doc", Some("qty"), make(Some("doc.party".into())));
        let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(read(&m).as_deref(), Some("doc.party"));
        assert!(m.version() > v1);
        let v2 = m.version();
        c.set("doc", Some("qty"), make(None));
        let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(read(&m), None);
        assert!(m.version() > v2);
    }
}

#[test]
fn fetch_from_override_is_set_replaced_and_cleared() {
    let mut c = Customisations::new();
    c.set(
        "doc",
        Some("tax"),
        Property::FetchFrom(Some("party.code".into())),
    );
    let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
    let v1 = m.version();
    assert_eq!(
        m.field_rules("doc", "tax")
            .and_then(|r| r.fetch_from)
            .as_deref(),
        Some("party.code")
    );
    c.set(
        "doc",
        Some("tax"),
        Property::FetchFrom(Some("party.tax_id".into())),
    );
    let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(m.version() > v1);
    assert_eq!(
        m.field_rules("doc", "tax")
            .and_then(|r| r.fetch_from)
            .as_deref(),
        Some("party.tax_id")
    );
    c.set("doc", Some("tax"), Property::FetchFrom(None));
    let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(m.field_rules("doc", "tax").and_then(|r| r.fetch_from), None);
}

#[test]
fn overriding_values_get_the_same_shape_checks() {
    let refused = |p: Property, f: &str| {
        let mut c = Customisations::new();
        c.set("doc", Some(f), p);
        let e = built(&c)
            .err()
            .unwrap_or_else(|| panic!("expected an error"));
        assert!(!e.is_empty());
        e
    };
    refused(Property::DependsOn(Some(" ".into())), "qty");
    refused(Property::MandatoryDependsOn(Some("a".repeat(1001))), "qty");
    refused(Property::FetchFrom(Some("nodot".into())), "tax");
    refused(Property::FetchFrom(Some("qty.tax_id".into())), "tax");
    // A type that cannot carry fetch_from is a refused override.
    let e = refused(Property::FetchFrom(Some("party.tax_id".into())), "party");
    assert_eq!(kinds(&e), [K::BadOverride]);
    // An expression default override.
    let mut c = Customisations::new();
    c.set(
        "doc",
        Some("qty"),
        Property::Default(Some("=doc.qty + 1".into())),
    );
    let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        m.field_rules("doc", "qty")
            .is_some_and(|r| r.default_is_expression())
    );
    // A field rule is not a property of a DocType.
    let mut c = Customisations::new();
    c.set("doc", None, Property::DependsOn(Some("x".into())));
    assert_eq!(
        built(&c).err().map(|e| kinds(&e)),
        Some(vec![K::BadOverride])
    );
}

#[test]
fn a_custom_field_may_carry_all_four() {
    let mut f = FieldSpec::new("custom_note", FieldType::Text);
    f.depends_on = Some("doc.qty > 0".into());
    f.mandatory_depends_on = Some("doc.qty > 1".into());
    f.read_only_depends_on = Some("doc.qty > 2".into());
    f.fetch_from = Some("party.code".into());
    let mut c = Customisations::new();
    c.add_field("doc", f, None);
    let m = built(&c).unwrap_or_else(|e| panic!("{e:?}"));
    let r = m
        .field_rules("doc", "custom_note")
        .unwrap_or_else(|| panic!("rules"));
    assert_eq!(r.depends_on.as_deref(), Some("doc.qty > 0"));
    assert_eq!(r.mandatory_depends_on.as_deref(), Some("doc.qty > 1"));
    assert_eq!(r.read_only_depends_on.as_deref(), Some("doc.qty > 2"));
    assert_eq!(r.fetch_from.as_deref(), Some("party.code"));

    // A custom field's bad fetch_from is refused like any other.
    let mut f = FieldSpec::new("custom_bad", FieldType::Text);
    f.fetch_from = Some("custom_missing.x".into());
    let mut c = Customisations::new();
    c.add_field("doc", f, None);
    assert!(built(&c).is_err());
}

#[test]
fn the_same_input_gives_the_same_app_view() {
    let make = || {
        let mut defs = doc(vec![]);
        field_mut(&mut defs, "qty").depends_on = Some("doc.tax".into());
        field_mut(&mut defs, "qty").default = Some("=1 + 1".into());
        field_mut(&mut defs, "tax").fetch_from = Some("party.code".into());
        defs[1].naming = Some("hash".into());
        ok(defs)
    };
    let (a, b) = (make(), make());
    assert_eq!(a.definitions(), b.definitions());
    assert_eq!(a.field_rules("doc", "qty"), b.field_rules("doc", "qty"));
    assert_eq!(a.doctype("doc"), b.doctype("doc"));
    assert_eq!(a.naming("doc"), b.naming("doc"));
}
