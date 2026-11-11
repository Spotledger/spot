//! Multiple selects and control hints (app API 0.4.0, ADR-068 amendment A2): the definition
//! properties, their validation, the site-override rules and the compiled app view.
#![allow(missing_docs)]

mod common;

use common::{build, kinds, simple};
use proptest::prelude::*;
use spot_app_api::meta::{ControlHint, FieldDef, FieldKind};
use spot_app_api::prelude::{Name, Value};
use spot_meta::descriptor::{Layer, Overridable, descriptor};
use spot_meta::{
    CONTROL_HINTS, Customisations, Definition, FieldSpec, FieldType, MetaErrorKind as K, Property,
    Registry,
};

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"

[[fields]]
name = "labels"
label = "Labels"
type = "select"
options = """
bug
feature
question
"""
multiple = true
control = "chips"
default = """
bug
"""

[[fields]]
name = "priority"
type = "select"
options = "low\nhigh"
default = "low"
control = "radio"
"#;

fn ticket() -> Definition {
    Definition::parse("ticket.toml", TICKET).unwrap_or_else(|e| panic!("{e}"))
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn select(field: &str, options: &str) -> FieldSpec {
    let mut f = FieldSpec::new(field, FieldType::Select);
    f.options = Some(options.to_owned());
    f
}

fn field(def: &Definition, field: &str) -> FieldSpec {
    def.fields
        .iter()
        .find(|f| f.name == field)
        .cloned()
        .unwrap_or_else(|| panic!("{field} is not a field"))
}

#[test]
fn both_properties_round_trip_from_toml_to_the_app_view() {
    let def = ticket();
    // The load kept both properties; the TOML writer writes them only when set.
    assert!(field(&def, "labels").multiple);
    assert_eq!(field(&def, "labels").control.as_deref(), Some("chips"));
    assert!(!field(&def, "priority").multiple);
    assert_eq!(field(&def, "priority").control.as_deref(), Some("radio"));
    let text = def.to_toml().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(text.matches("multiple").count(), 1, "{text}");
    assert_eq!(text.matches("control").count(), 2, "{text}");
    let back = Definition::parse("ticket.toml", &text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, def);

    // Validation accepts the definition and effective metadata keeps it.
    let meta = build(vec![back]).unwrap_or_else(|e| panic!("{e:?}"));
    let effective = meta
        .definition("ticket")
        .unwrap_or_else(|| panic!("ticket"));
    assert!(field(effective, "labels").multiple);
    assert_eq!(field(effective, "labels").control.as_deref(), Some("chips"));
    assert!(meta.warnings().is_empty(), "{:?}", meta.warnings());

    // Compile carries both into the app view.
    let view = meta.doctype("ticket").unwrap_or_else(|| panic!("ticket"));
    let labels = view.field("labels").unwrap_or_else(|| panic!("labels"));
    assert!(labels.multiple);
    assert_eq!(labels.hints.control, Some(ControlHint::Chips));
    // A multiple select's list default is not a scalar: the view carries none.
    assert_eq!(labels.default, None);
    let priority = view.field("priority").unwrap_or_else(|| panic!("priority"));
    assert!(!priority.multiple);
    assert_eq!(priority.hints.control, Some(ControlHint::Radio));
    assert_eq!(priority.default, Some(Value::Text("low".into())));
}

#[test]
fn multiple_is_only_for_a_select() {
    let mut f = FieldSpec::new("body", FieldType::Text);
    f.multiple = true;
    let e = build(vec![simple("note", vec![f])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadProperty]);
    assert!(e[0].message.contains("multiple"), "{}", e[0].message);
    assert!(e[0].message.contains("select"), "{}", e[0].message);
}

#[test]
fn a_multiple_select_cannot_be_unique() {
    let mut f = select("labels", "bug\nfeature");
    f.multiple = true;
    f.unique = true;
    let e = build(vec![simple("note", vec![f])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadProperty]);
    assert!(e[0].message.contains("unique"), "{}", e[0].message);
}

#[test]
fn a_multiple_select_default_must_be_a_list_of_its_options() {
    let scalar = |default: &str| {
        let mut f = select("labels", "bug\nfeature");
        f.multiple = true;
        f.default = Some(default.to_owned());
        f
    };
    // A scalar default is refused: the stored value is an array.
    let e = build(vec![simple("note", vec![scalar("bug")])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadDefault]);
    assert!(e[0].message.contains("scalar"), "{}", e[0].message);
    // An expression is a scalar too.
    let e = build(vec![simple("note", vec![scalar("=doc.pick")])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadDefault]);
    // Every value of the list must be one of the options.
    let e = build(vec![simple("note", vec![scalar("bug\nother\n")])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadDefault]);
    assert!(e[0].message.contains("other"), "{}", e[0].message);
    // A value listed twice is refused.
    let e = build(vec![simple("note", vec![scalar("bug\nbug\n")])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadDefault]);
    assert!(e[0].message.contains("twice"), "{}", e[0].message);
    // A list is accepted, including a one-value list (a trailing line break makes it a list).
    let mut list = scalar("bug\n");
    assert!(field(&simple("note", vec![list.clone()]), "labels").multiple);
    list.default = Some("bug\nfeature\n".into());
    let meta = build(vec![simple("note", vec![list])]).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(meta.warnings().is_empty());
}

#[test]
fn every_multiple_problem_is_reported_at_once() {
    let mut wrong = FieldSpec::new("title", FieldType::Text);
    wrong.multiple = true;
    let mut unique = select("state", "open\nclosed");
    unique.multiple = true;
    unique.unique = true;
    let mut scalar = select("kind", "one\ntwo");
    scalar.multiple = true;
    scalar.default = Some("one".into());
    let e = build(vec![simple("note", vec![wrong, unique, scalar])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadProperty, K::BadProperty, K::BadDefault]);
}

#[test]
fn an_unknown_control_is_a_typed_load_error_that_lists_the_valid_names() {
    let text = "name = \"note\"\n[[fields]]\nname = \"state\"\ntype = \"select\"\n\
                options = \"open\\nclosed\"\ncontrol = \"slider\"\n";
    let e = Definition::parse("note.toml", text)
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(e.kind, K::Parse);
    assert!(e.message.contains("slider"), "{}", e.message);
    for hint in CONTROL_HINTS {
        assert!(e.message.contains(hint), "{} misses {hint}", e.message);
    }
    assert!(e.to_string().contains("note.toml"), "{e}");

    // A spec built in code is refused by validation with the same remedy, never dropped.
    let mut f = select("state", "open\nclosed");
    f.control = Some("slider".into());
    let e = build(vec![simple("note", vec![f])])
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(kinds(&e), [K::BadProperty]);
    for hint in CONTROL_HINTS {
        assert!(
            e[0].message.contains(hint),
            "{} misses {hint}",
            e[0].message
        );
    }
}

#[test]
fn a_control_that_cannot_apply_is_kept_and_reported_as_a_warning() {
    let mut f = select("state", "open\nclosed");
    f.control = Some("switch".into());
    let meta = build(vec![simple("note", vec![f])]).unwrap_or_else(|e| panic!("{e:?}"));
    // The hint survives into the app view; the client falls back.
    let view = meta.doctype("note").unwrap_or_else(|| panic!("note"));
    assert_eq!(
        view.field("state").and_then(|f| f.hints.control),
        Some(ControlHint::Switch)
    );
    let warnings = meta.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "meta.control_not_applicable");
    assert_eq!(warnings[0].doctype, "note");
    assert_eq!(warnings[0].field.as_deref(), Some("state"));
    assert!(
        warnings[0].message.contains("switch"),
        "{}",
        warnings[0].message
    );
    assert!(!meta.report().is_empty());

    // A hint that does not fit the multi-valued shape warns too.
    let mut f = select("labels", "bug\nfeature");
    f.multiple = true;
    f.control = Some("radio".into());
    let meta = build(vec![simple("note", vec![f])]).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(meta.warnings().len(), 1, "{:?}", meta.warnings());
}

#[test]
fn a_control_that_fits_is_not_reported() {
    let mut flag = FieldSpec::new("active", FieldType::Bool);
    flag.control = Some("switch".into());
    let mut state = select("state", "open\nclosed");
    state.control = Some("dropdown".into());
    let mut labels = select("labels", "bug\nfeature");
    labels.multiple = true;
    labels.control = Some("chips".into());
    let mut body = FieldSpec::new("body", FieldType::Text);
    body.control = Some("textarea".into());
    let mut notes = FieldSpec::new("notes", FieldType::LongText);
    notes.control = Some("textarea".into());
    let mut target_type = FieldSpec::new("target_type", FieldType::Text);
    target_type.label = Some("Target Type".into());
    let mut dynamic = FieldSpec::new("target", FieldType::DynamicLink);
    dynamic.options = Some("target_type".into());
    dynamic.control = Some("autocomplete".into());
    let mut party = FieldSpec::new("party", FieldType::Link);
    party.options = Some("customer".into());
    party.control = Some("autocomplete".into());

    let mut customer = Definition::new("customer");
    customer.fields = vec![FieldSpec::new("customer_name", FieldType::Text)];
    let note = simple(
        "note",
        vec![
            flag,
            state,
            labels,
            body,
            notes,
            target_type,
            dynamic,
            party,
        ],
    );
    let meta = build(vec![customer, note]).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(meta.warnings().is_empty(), "{:?}", meta.warnings());
}

#[test]
fn control_is_overridable_and_multiple_is_definition_only() {
    let mut c = Customisations::new();
    c.set(
        "ticket",
        Some("priority"),
        Property::parse("control", "segmented").unwrap_or_else(|e| panic!("{e}")),
    );
    // Clearing the hint is an override too.
    c.set("ticket", Some("labels"), Property::Control(None));
    let meta = Registry::build(vec![ticket()], &c).unwrap_or_else(|e| panic!("{e:?}"));

    let effective = meta
        .definition("ticket")
        .unwrap_or_else(|| panic!("ticket"));
    assert_eq!(
        field(effective, "priority").control.as_deref(),
        Some("segmented")
    );
    assert_eq!(field(effective, "labels").control, None);
    // `multiple` is not a layered property: the definition's value is the effective value.
    assert!(field(effective, "labels").multiple);

    let view = meta.doctype("ticket").unwrap_or_else(|| panic!("ticket"));
    assert_eq!(
        view.field("priority").and_then(|f| f.hints.control),
        Some(ControlHint::Segmented)
    );
    assert_eq!(view.field("labels").and_then(|f| f.hints.control), None);
    assert!(
        view.field("labels").is_some_and(|f| f.multiple),
        "the override did not change the stored shape"
    );

    // A site cannot express `multiple`: the descriptor refuses it with its layer.
    let e = Property::parse("multiple", "true")
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(e.kind, K::BadOverride);
    assert!(e.message.contains("multiple"), "{}", e.message);
    assert!(e.message.contains("storage"), "{}", e.message);

    // An unknown control name in an override is refused with the list of valid ones.
    let e = Property::parse("control", "slider")
        .err()
        .unwrap_or_else(|| panic!("expected an error"));
    assert_eq!(e.kind, K::BadOverride);
    for hint in CONTROL_HINTS {
        assert!(e.message.contains(hint), "{} misses {hint}", e.message);
    }
}

#[test]
fn a_site_may_set_an_unfitting_control_and_only_the_report_mentions_it() {
    let mut c = Customisations::new();
    c.set(
        "ticket",
        Some("labels"),
        Property::Control(Some("switch".into())),
    );
    let meta = Registry::build(vec![ticket()], &c).unwrap_or_else(|e| panic!("{e:?}"));
    let view = meta.doctype("ticket").unwrap_or_else(|| panic!("ticket"));
    assert_eq!(
        view.field("labels").and_then(|f| f.hints.control),
        Some(ControlHint::Switch)
    );
    assert_eq!(meta.warnings().len(), 1, "{:?}", meta.warnings());
    assert_eq!(meta.warnings()[0].field.as_deref(), Some("labels"));
}

#[test]
fn the_new_properties_are_described() {
    let multiple = descriptor("multiple").unwrap_or_else(|| panic!("multiple"));
    assert_eq!(multiple.layer, Layer::Storage);
    assert_eq!(multiple.overridable, Overridable::Never);
    assert!(!multiple.doc.is_empty(), "multiple has no documentation");
    assert!(multiple.applies_to.accepts(FieldType::Select, None));
    assert!(!multiple.applies_to.accepts(FieldType::Text, None));

    let control = descriptor("control").unwrap_or_else(|| panic!("control"));
    assert_eq!(control.layer, Layer::Presentation);
    assert_eq!(control.overridable, Overridable::SiteAndView);
    assert!(!control.doc.is_empty(), "control has no documentation");
    assert!(control.applies_to.accepts(FieldType::Json, None));
}

#[test]
fn a_definition_without_the_new_properties_keeps_the_old_view() {
    let mut status = select("status", "Open\nClosed");
    status.default = Some("Open".into());
    let meta = build(vec![simple("note", vec![status])]).unwrap_or_else(|e| panic!("{e:?}"));
    let view = meta.doctype("note").unwrap_or_else(|| panic!("note"));

    // Byte-for-byte the v0.1 shape: the field is exactly what the 0.3.0 constructor makes,
    // with the scalar default compile has always set.
    let mut expected = FieldDef::new(
        name("status"),
        FieldKind::Select {
            values: vec!["Open".into(), "Closed".into()],
        },
        false,
        false,
    );
    expected.default = Some(Value::Text("Open".into()));
    assert_eq!(view.fields, vec![expected]);
    let shown = format!("{:?}", view.fields[0]);
    assert!(shown.contains("multiple: false"), "{shown}");
    assert!(shown.contains("control: None"), "{shown}");
    assert!(meta.report().is_empty());
}

proptest! {
    /// Arbitrary control text never panics the loader: it loads, or it is a typed error.
    #[test]
    fn arbitrary_control_text_never_panics(raw in any::<String>()) {
        let quoted = toml::Value::String(raw.clone()).to_string();
        let text = format!(
            "name = \"note\"\n[[fields]]\nname = \"state\"\ntype = \"select\"\n\
             options = \"open\\nclosed\"\ncontrol = {quoted}\n"
        );
        match Definition::parse("note.toml", &text) {
            Ok(def) => {
                // A value that loads is one the definition format knows.
                let control = &def.fields[0].control;
                prop_assert!(control.as_deref().is_none_or(|c| CONTROL_HINTS.contains(&c)));
            }
            Err(e) => prop_assert_eq!(e.kind, K::Parse),
        }
    }

    /// The same for `multiple`: any TOML value loads or is a typed error, never a panic.
    #[test]
    fn arbitrary_multiple_and_control_toml_never_panics(
        value in any::<Option<bool>>(),
        raw in any::<String>(),
    ) {
        let multiple = value.map_or_else(|| "1".to_owned(), |b| b.to_string());
        let text = format!(
            "name = \"note\"\n[[fields]]\nname = \"state\"\ntype = \"select\"\n\
             options = \"open\\nclosed\"\nmultiple = {multiple}\ncontrol = {}\n",
            toml::Value::String(raw)
        );
        match Definition::parse("note.toml", &text) {
            Ok(def) => {
                prop_assert_eq!(def.fields[0].multiple, value.unwrap_or(false));
            }
            Err(e) => prop_assert_eq!(e.kind, K::Parse),
        }
    }
}
