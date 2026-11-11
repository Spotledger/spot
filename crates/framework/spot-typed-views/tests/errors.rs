//! GenError tests: invalid names, collisions, deterministic ordering — and the custom-field
//! story of the brief, on the generator's side: a `custom_*` field in the *definitions* is
//! generated like any other field, while a field added to a document at runtime is never in
//! the definitions and stays reachable only through `.doc()` (proven in `generated.rs`).

use spot_meta::{FieldSpec, FieldType};
use spot_typed_views::{GenErrorKind, generate};

/// Parses definitions, reporting every error.
fn parse(
    definitions: &[spot_meta::Definition],
) -> Result<spot_typed_views::Model, Vec<spot_typed_views::GenError>> {
    spot_typed_views::parse(definitions)
}

/// The 3 sample DocTypes of `tests/fixtures/doctypes/`, loaded.
///
/// # Panics
/// Checked-in fixtures are valid; a load failure is a setup bug.
#[must_use]
pub fn definitions() -> Vec<spot_meta::Definition> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/doctypes");
    spot_meta::load_dir(&dir).unwrap_or_else(|e| panic!("fixtures no longer load: {e:?}"))
}

/// An invalid field name: `InvalidName`, naming the DocType and the field.
#[test]
fn invalid_field_name_is_invalid_name() {
    let mut def = spot_meta::Definition::new("order");
    def.fields = vec![FieldSpec::new("ship To", FieldType::Text)];
    let errors = parse(&[def]).err().unwrap_or_else(|| panic!("must fail"));
    assert_eq!(1, errors.len());
    assert_eq!(GenErrorKind::InvalidName, errors[0].kind);
    assert_eq!(Some("order".to_owned()), errors[0].doctype);
    assert_eq!(Some("ship To".to_owned()), errors[0].field);
    assert!(
        errors[0].message.contains("not a snake_case name"),
        "{:?}",
        errors[0]
    );
}

/// An invalid DocType name: `InvalidName`, naming the DocType; `generate` reports it too.
#[test]
fn invalid_doctype_name_is_invalid_name() {
    let mut def = spot_meta::Definition::new("Order");
    def.fields = vec![FieldSpec::new("code", FieldType::Text)];
    let errors = parse(&[def.clone()])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(1, errors.len());
    assert_eq!(GenErrorKind::InvalidName, errors[0].kind);
    assert_eq!(Some("Order".to_owned()), errors[0].doctype);
    assert!(
        generate(&[def]).is_err(),
        "generate reports the same errors"
    );
}

/// A select field without options: `BadField`; a table field without a child DocType too.
#[test]
fn select_and_table_without_options_are_bad_fields() {
    let mut no_select = spot_meta::Definition::new("order");
    no_select.fields = vec![FieldSpec::new("status", FieldType::Select)];
    let errors = parse(&[no_select])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(GenErrorKind::BadField, errors[0].kind);

    let mut no_table = spot_meta::Definition::new("order");
    no_table.fields = vec![FieldSpec::new("lines", FieldType::Table)];
    let errors = parse(&[no_table])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(GenErrorKind::BadField, errors[0].kind);

    let mut unknown_child = spot_meta::Definition::new("order");
    let mut lines = FieldSpec::new("lines", FieldType::Table);
    lines.options = Some("no_such_child".to_owned());
    unknown_child.fields = vec![lines];
    let errors = parse(&[unknown_child])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(GenErrorKind::BadField, errors[0].kind);
    assert!(
        errors[0].message.contains("no_such_child"),
        "{:?}",
        errors[0]
    );
}

/// Two fields mapping onto one identifier collide: the second `ship_to` lands on the same
/// member as the first (and `validate_snake` keeps `__`-runs out before that, since a
/// double underscore is not a word boundary a source name can carry).
#[test]
fn colliding_field_names_are_a_collision() {
    let mut def = spot_meta::Definition::new("order");
    def.fields = vec![
        FieldSpec::new("ship_to", FieldType::Text),
        FieldSpec::new("ship_to", FieldType::Int),
    ];
    let errors = parse(&[def]).err().unwrap_or_else(|| panic!("must fail"));
    assert_eq!(1, errors.len());
    assert_eq!(GenErrorKind::Collision, errors[0].kind);
    assert!(errors[0].message.contains("ship_to"), "{:?}", errors[0]);
}

/// A field named `type` becomes `r#type` in the getter, `set_type` in the setter: the raw
/// identifier keeps the storage spelling (`set_` needs no prefix, `type` is one word).
#[test]
fn keyword_field_maps_onto_a_raw_identifier() {
    let mut def = spot_meta::Definition::new("form");
    def.fields = vec![FieldSpec::new("type", FieldType::Text)];
    let source = generate(&[def]).unwrap_or_else(|e| panic!("generate: {e:?}"));
    assert!(
        source.contains("pub fn r#type(&self)"),
        "keyword field must be a raw identifier:\n{source}"
    );
    assert!(
        source.contains("pub fn set_type(&mut self"),
        "setter keeps the keyword spelling:\n{source}"
    );
    assert!(
        source.contains("self.0.text(\"type\")"),
        "the accessor still addresses the storage name:\n{source}"
    );
}

/// A field whose name a raw identifier cannot spell (`self`, `Self`, primitives) is refused
/// with `InvalidName`, not mangled and never aliased.
#[test]
fn unspellable_field_names_are_refused() {
    for name in ["self", "Self", "crate", "bool"] {
        let mut def = spot_meta::Definition::new("form");
        def.fields = vec![FieldSpec::new(name, FieldType::Text)];
        let errors = parse(&[def])
            .err()
            .unwrap_or_else(|| panic!("`{name}` must fail"));
        assert_eq!(GenErrorKind::InvalidName, errors[0].kind, "`{name}`");
    }
}

/// Every error comes out at once, sorted, and the same input twice yields the same list, so
/// build logs are stable: two DocTypes, each with one bad select, plus two invalid names.
#[test]
fn every_error_is_reported_and_sorted() {
    let mut first = spot_meta::Definition::new("order");
    first.fields = vec![
        FieldSpec::new("status", FieldType::Select), // BadField: no options
        FieldSpec::new("bad name", FieldType::Text), // InvalidName
        FieldSpec::new("self", FieldType::Text),     // InvalidName: unspellable
    ];
    let mut second = spot_meta::Definition::new("invoice");
    second.fields = vec![
        FieldSpec::new("status", FieldType::Select), // BadField: no options
    ];
    let errors = parse(&[first, second])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert!(errors.len() >= 3, "every error at once: {errors:?}");
    let mut sorted = errors.clone();
    sorted.sort();
    assert_eq!(errors, sorted, "errors come out sorted: {errors:?}");
    // And the same input the second time gives the same list.
    let again = {
        let mut first = spot_meta::Definition::new("order");
        first.fields = vec![
            FieldSpec::new("status", FieldType::Select),
            FieldSpec::new("bad name", FieldType::Text),
            FieldSpec::new("self", FieldType::Text),
        ];
        let mut second = spot_meta::Definition::new("invoice");
        second.fields = vec![FieldSpec::new("status", FieldType::Select)];
        parse(&[first, second]).err().unwrap_or_default()
    };
    assert_eq!(errors, again, "deterministic");
}

/// A `custom_*` field present in the definitions at build time is a standard field to the
/// generator: typed getters and setters like any other. (The runtime-added field the brief
/// describes is reached through `.doc()`, tested in `generated.rs`.)
#[test]
fn custom_field_in_definitions_is_generated_normally() {
    let mut def = spot_meta::Definition::new("order");
    def.fields = vec![
        FieldSpec::new("code", FieldType::Text),
        FieldSpec::new("custom_channel", FieldType::Text),
    ];
    let source = generate(&[def]).unwrap_or_else(|e| panic!("generate: {e:?}"));
    assert!(
        source.contains("pub fn custom_channel(&self)"),
        "a definition-time custom field is generated:\n{source}"
    );
    assert!(
        definitions()
            .iter()
            .all(|d| { d.fields.iter().all(|f| !f.name.starts_with("custom_")) }),
        "the fixtures carry no custom fields: the runtime field is invisible to the generator"
    );
}
