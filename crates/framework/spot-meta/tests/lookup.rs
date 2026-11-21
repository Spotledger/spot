//! The `lookup` property (ADR-068 §3): the definition format, validation, the known-seam check,
//! the site override and the app view. Nothing here evaluates a lookup or calls a seam.
#![allow(missing_docs)]

mod common;

use std::collections::BTreeSet;

use common::{build, kinds, of, simple, text};
use spot_app_api::lookup::{LOOKUP_VERSION, PLATFORM_LINK_SEARCH};
use spot_meta::{
    BuildOptions, Customisations, Definition, EffectiveMeta, FieldLookup, FieldSpec, FieldType,
    LookupRule, LookupSpec, MetaError, MetaErrorKind as K, Property, Registry,
};

fn child(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = simple(name, fields);
    d.is_child = true;
    d
}

/// A DocType `thing` with one field of every type that may carry a lookup, plus some that may
/// not, and the DocTypes they point at.
fn world() -> Vec<Definition> {
    vec![
        simple("target", vec![text("name")]),
        simple(
            "thing",
            vec![
                text("kind"),
                of(FieldType::Link, "owner", Some("target")),
                of(FieldType::DynamicLink, "about", Some("kind")),
                of(FieldType::Select, "pick", Some("a\nb")),
                of(FieldType::TableMultiselect, "tags", Some("thing_tag")),
                text("note"),
                FieldSpec::new("count", FieldType::Int),
                FieldSpec::new("day", FieldType::Date),
            ],
        ),
        child(
            "thing_tag",
            vec![of(FieldType::Link, "tag", Some("target"))],
        ),
    ]
}

fn with_lookup(field: &str, spec: LookupSpec) -> Vec<Definition> {
    let mut defs = world();
    for d in &mut defs {
        for f in &mut d.fields {
            if d.name == "thing" && f.name == field {
                f.lookup = Some(FieldLookup::Spec(spec.clone()));
            }
        }
    }
    defs
}

fn built(defs: Vec<Definition>) -> EffectiveMeta {
    build(defs).unwrap_or_else(|e| panic!("{e:?}"))
}

fn errors(defs: Vec<Definition>) -> Vec<MetaError> {
    build(defs).err().unwrap_or_else(|| panic!("must fail"))
}

fn app_lookup(m: &EffectiveMeta, field: &str) -> Option<spot_app_api::lookup::LookupSpec> {
    let view = m.doctype("thing").unwrap_or_else(|| panic!("thing"));
    view.fields
        .iter()
        .find(|f| f.name.as_str() == field)
        .unwrap_or_else(|| panic!("{field}"))
        .lookup
        .clone()
}

fn spec(seam: &str) -> LookupSpec {
    LookupSpec::seam(seam)
}

#[test]
fn every_type_that_accepts_a_lookup_round_trips_to_the_app_view() {
    for field in ["owner", "about", "pick", "tags", "note"] {
        let mut s = spec("billing:customer_search");
        s.params.insert("only_active".into(), "true".into());
        let m = built(with_lookup(field, s));
        let got = app_lookup(&m, field).unwrap_or_else(|| panic!("{field} lost its lookup"));
        assert_eq!(
            got.seam.as_deref(),
            Some("billing:customer_search"),
            "{field}"
        );
        assert_eq!(got.version, LOOKUP_VERSION, "{field}");
        assert_eq!(
            got.params.get("only_active").map(String::as_str),
            Some("true")
        );
        assert!(got.when.is_empty());
    }
    // No lookup is the platform default: the view carries none.
    assert_eq!(app_lookup(&built(world()), "owner"), None);
}

#[test]
fn a_lookup_without_a_seam_means_the_platform_default() {
    let mut s = LookupSpec::default();
    s.params.insert("limit".into(), "5".into());
    let m = built(with_lookup("owner", s));
    let got = app_lookup(&m, "owner").unwrap_or_else(|| panic!("lookup"));
    assert_eq!(got.seam, None);
    assert_eq!(
        spot_app_api::lookup::lookup_seam(&got),
        PLATFORM_LINK_SEARCH
    );
    assert_eq!(got.params.len(), 1);
}

#[test]
fn the_other_field_types_refuse_a_lookup_naming_the_field() {
    for field in ["count", "day"] {
        let e = errors(with_lookup(field, spec("billing:x")));
        assert_eq!(kinds(&e), [K::BadProperty], "{field}: {e:?}");
        assert_eq!(e[0].doctype.as_deref(), Some("thing"));
        assert_eq!(e[0].field.as_deref(), Some(field));
        assert!(e[0].message.contains("lookup"), "{}", e[0]);
    }
}

#[test]
fn seam_names_are_app_colon_name_in_storage_characters() {
    for ok in ["billing:customer_search", "a:b", "platform:link_search"] {
        built(with_lookup("owner", spec(ok)));
    }
    for bad in [
        "customer_search",
        "billing:",
        ":search",
        "Billing:search",
        "bill-ing:search",
        "billing:two:parts",
        "billing:Search",
        "",
    ] {
        let e = errors(with_lookup("owner", spec(bad)));
        assert_eq!(kinds(&e), [K::BadProperty], "`{bad}`: {e:?}");
        assert_eq!(e[0].field.as_deref(), Some("owner"));
    }
}

#[test]
fn the_platform_prefix_is_reserved_for_the_standard_seam() {
    let e = errors(with_lookup("owner", spec("platform:everything")));
    assert_eq!(kinds(&e), [K::BadProperty], "{e:?}");
    assert!(e[0].message.contains("platform:link_search"), "{}", e[0]);
}

#[test]
fn a_select_with_a_lookup_may_have_no_options_and_without_one_it_may_not() {
    let sel = |lookup: Option<LookupSpec>| {
        let mut f = FieldSpec::new("pick", FieldType::Select);
        f.lookup = lookup.map(FieldLookup::Spec);
        build(vec![simple("thing", vec![f])])
    };
    assert!(sel(Some(spec("billing:picks"))).is_ok());
    // Without a lookup a select still needs its options (absent or empty is refused).
    let e = sel(None).err().unwrap_or_default();
    assert!(!e.is_empty());
    assert!(
        e.iter().all(|x| x.field.as_deref() == Some("pick")),
        "{e:?}"
    );
}

#[test]
fn when_rules_are_parsed_and_kept_but_not_accepted_yet() {
    let text = r#"
name = "thing"
[[fields]]
name = "owner"
type = "link"
options = "thing"
[fields.lookup]
seam = "billing:customer_search"
params = { only_active = "true" }
[[fields.lookup.when]]
if = "doc.order_type == 'export'"
seam = "billing:export_customer_search"
"#;
    let d = Definition::parse("thing.toml", text).unwrap_or_else(|e| panic!("{e}"));
    let lookup = d.fields[0]
        .lookup
        .clone()
        .unwrap_or_else(|| panic!("lookup parsed"));
    let lookup = lookup
        .spec()
        .unwrap_or_else(|| panic!("the flag must not parse here"));
    assert_eq!(lookup.when.len(), 1);
    assert_eq!(lookup.when[0].if_expr, "doc.order_type == 'export'");
    assert_eq!(lookup.when[0].seam, "billing:export_customer_search");
    // Kept: the format survives a write and a read.
    let again = Definition::parse("thing.toml", &d.to_toml().unwrap_or_else(|e| panic!("{e}")))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(again, d);
    // Not accepted: it waits for the expression language.
    let e = errors(vec![d]);
    assert_eq!(kinds(&e), [K::NotYetSupported], "{e:?}");
    assert_eq!(e[0].doctype.as_deref(), Some("thing"));
    assert_eq!(e[0].field.as_deref(), Some("owner"));
    assert!(e[0].message.contains("WP-UI-11"), "{}", e[0]);
}

#[test]
fn a_rule_seam_is_checked_like_any_seam() {
    let mut s = spec("billing:customer_search");
    s.when.push(LookupRule {
        if_expr: "true".into(),
        seam: "nope".into(),
        ..LookupRule::default()
    });
    let e = errors(with_lookup("owner", s));
    assert!(kinds(&e).contains(&K::NotYetSupported));
    assert!(kinds(&e).contains(&K::BadProperty));
}

#[test]
fn unknown_keys_in_a_lookup_are_a_parse_error() {
    let text = "name = \"thing\"\n[[fields]]\nname = \"owner\"\ntype = \"link\"\noptions = \"thing\"\n[fields.lookup]\nseam = \"a:b\"\nfoo = 1\n";
    let e = Definition::parse("thing.toml", text).err();
    assert_eq!(e.map(|e| e.kind), Some(K::Parse));
}

fn known(seams: &[&str]) -> BuildOptions {
    let mut o = BuildOptions::new();
    o.known_seams = Some(
        seams
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>(),
    );
    o
}

#[test]
fn the_known_seam_set_is_only_checked_when_given() {
    let defs = with_lookup("owner", spec("billing:ghost"));
    // `build` and `build_with` without a set do not check.
    assert!(Registry::build(defs.clone(), &Customisations::new()).is_ok());
    assert!(
        Registry::build_with(defs.clone(), &Customisations::new(), &BuildOptions::new()).is_ok()
    );
    // With a set, a seam outside it is an error naming DocType, field and seam.
    let e = Registry::build_with(defs.clone(), &Customisations::new(), &known(&["a:b"]))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&e), [K::UnknownSeam], "{e:?}");
    assert_eq!(e[0].doctype.as_deref(), Some("thing"));
    assert_eq!(e[0].field.as_deref(), Some("owner"));
    assert!(e[0].message.contains("billing:ghost"), "{}", e[0]);
    // A seam in the set passes, and so does the standard one even if the set omits it.
    assert!(Registry::build_with(defs, &Customisations::new(), &known(&["billing:ghost"])).is_ok());
    let std = with_lookup("owner", spec(PLATFORM_LINK_SEARCH));
    assert!(Registry::build_with(std, &Customisations::new(), &known(&[])).is_ok());
}

#[test]
fn a_custom_field_naming_an_unknown_seam_is_refused() {
    let mut f = of(FieldType::Link, "custom_boss", Some("target"));
    f.lookup = Some(FieldLookup::Spec(spec("billing:ghost")));
    let mut c = Customisations::new();
    c.add_field("thing", f, None);
    let opts = known(&["billing:customer_search"]);
    let e = Registry::build_with(world(), &c, &opts)
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&e), [K::UnknownSeam], "{e:?}");
    assert_eq!(e[0].field.as_deref(), Some("custom_boss"));
    // Known: the custom field's lookup reaches the app view.
    let ok = known(&["billing:ghost"]);
    let m = Registry::build_with(world(), &c, &ok).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        app_lookup(&m, "custom_boss").and_then(|l| l.seam),
        Some("billing:ghost".to_owned())
    );
}

#[test]
fn a_site_override_sets_replaces_and_clears_the_lookup() {
    let mut defs = with_lookup("owner", {
        let mut s = spec("billing:customer_search");
        s.params.insert("only_active".into(), "true".into());
        s
    });
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut registry = Registry::new(defs).unwrap_or_else(|e| panic!("{e:?}"));
    let v0 = registry.effective().version();

    // Replace: the spec is replaced as a whole, so the definition's params are gone.
    let v1 = registry
        .set_property(
            "thing",
            Some("owner"),
            Property::Lookup(Some(spec("sales:export_search"))),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(v1 > v0);
    let got = app_lookup(&registry.effective(), "owner").unwrap_or_else(|| panic!("lookup"));
    assert_eq!(got.seam.as_deref(), Some("sales:export_search"));
    assert!(got.params.is_empty(), "no field-by-field merge");

    // Set on a field that had none.
    let v2 = registry
        .set_property(
            "thing",
            Some("pick"),
            Property::Lookup(Some(spec("sales:picks"))),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(v2 > v1);
    assert!(app_lookup(&registry.effective(), "pick").is_some());

    // Clear: the platform default, which the view shows as none.
    let v3 = registry
        .set_property("thing", Some("owner"), Property::Lookup(None))
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(v3 > v2);
    assert_eq!(app_lookup(&registry.effective(), "owner"), None);
}

#[test]
fn the_override_gets_the_same_validation_and_a_failed_one_changes_nothing() {
    let mut registry = Registry::new(world()).unwrap_or_else(|e| panic!("{e:?}"));
    let before = registry.effective().version();
    for (field, spec) in [
        ("owner", spec("bad name")),
        ("owner", spec("platform:other")),
        ("count", spec("billing:x")),
    ] {
        let e = registry
            .set_property("thing", Some(field), Property::Lookup(Some(spec)))
            .err()
            .unwrap_or_else(|| panic!("{field} must be refused"));
        assert!(!e.is_empty());
    }
    let mut with_when = spec("billing:x");
    with_when.when.push(LookupRule {
        if_expr: "true".into(),
        seam: "billing:y".into(),
        ..LookupRule::default()
    });
    let e = registry
        .set_property("thing", Some("owner"), Property::Lookup(Some(with_when)))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&e), [K::NotYetSupported]);
    assert_eq!(registry.effective().version(), before);
}

#[test]
fn a_registry_keeps_its_known_seams_for_later_changes() {
    let mut registry = Registry::with_options(world(), known(&["billing:customer_search"]))
        .unwrap_or_else(|e| panic!("{e:?}"));
    let e = registry
        .set_property(
            "thing",
            Some("owner"),
            Property::Lookup(Some(spec("billing:ghost"))),
        )
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&e), [K::UnknownSeam]);
    assert!(
        registry
            .set_property(
                "thing",
                Some("owner"),
                Property::Lookup(Some(spec("billing:customer_search"))),
            )
            .is_ok()
    );
}

#[test]
fn the_text_form_of_the_property_parses_like_a_definition() {
    let p = Property::parse(
        "lookup",
        r#"seam = "billing:customer_search", params = { only_active = "true" }"#,
    );
    let Ok(Property::Lookup(Some(got))) = p else {
        panic!("{p:?}");
    };
    assert_eq!(got.seam.as_deref(), Some("billing:customer_search"));
    assert_eq!(got.params.len(), 1);
    assert_eq!(Property::parse("lookup", ""), Ok(Property::Lookup(None)));
    let e = Property::parse("lookup", "colour = 1").err();
    assert_eq!(e.map(|e| e.kind), Some(K::BadOverride));
}

#[test]
fn the_same_input_gives_the_same_app_view() {
    let defs = with_lookup("owner", {
        let mut s = spec("billing:customer_search");
        s.params.insert("b".into(), "2".into());
        s.params.insert("a".into(), "1".into());
        s
    });
    let a = built(defs.clone());
    let b = built(defs);
    let (va, vb) = (
        a.doctype("thing").unwrap_or_else(|| panic!("thing")),
        b.doctype("thing").unwrap_or_else(|| panic!("thing")),
    );
    assert_eq!(format!("{va:?}"), format!("{vb:?}"));
    assert_eq!(va, vb);
}
