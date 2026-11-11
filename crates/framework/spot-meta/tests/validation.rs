//! Every validation error, and that all of them are reported at once.
#![allow(missing_docs)]

mod common;

use common::{build, effective, fixtures, kinds, of, simple, text};
use spot_meta::{Definition, FieldSpec, FieldType, IndexSpec, MetaError, MetaErrorKind as K};

fn fails(defs: Vec<Definition>) -> Vec<MetaError> {
    build(defs)
        .err()
        .unwrap_or_else(|| panic!("expected validation errors"))
}

fn has(errors: &[MetaError], kind: K, doctype: &str, field: Option<&str>) -> bool {
    errors.iter().any(|e| {
        e.kind == kind && e.doctype.as_deref() == Some(doctype) && e.field.as_deref() == field
    })
}

fn with_field(f: FieldSpec) -> Vec<Definition> {
    vec![simple("thing", vec![f])]
}

#[test]
fn fixtures_are_valid() {
    let _ = effective();
}

#[test]
fn storage_names_are_required() {
    let e = fails(vec![simple("Bad Name", vec![text("Also-Bad")])]);
    assert!(has(&e, K::InvalidName, "Bad Name", None));
    assert!(has(&e, K::InvalidName, "Bad Name", Some("Also-Bad")));
}

#[test]
fn system_columns_and_engine_fields_are_reserved() {
    let fields = [
        "id",
        "version",
        "parent",
        "parent_field",
        "idx",
        "doc_state",
        "amended_from",
    ];
    for name in fields {
        let e = fails(with_field(text(name)));
        assert!(
            has(&e, K::ReservedName, "thing", Some(name)),
            "{name}: {e:?}"
        );
    }
    // Engine fields are reserved on every DocType, posted or not.
    let mut posted = simple("p", vec![text("doc_state")]);
    posted.is_posted = true;
    assert!(has(
        &fails(vec![posted]),
        K::ReservedName,
        "p",
        Some("doc_state")
    ));
}

#[test]
fn duplicates_are_reported() {
    let e = fails(vec![
        simple("a", vec![text("x"), text("x")]),
        simple("a", vec![]),
    ]);
    assert!(has(&e, K::Duplicate, "a", Some("x")));
    assert!(has(&e, K::Duplicate, "a", None));
}

#[test]
fn links_must_point_at_existing_non_child_doctypes() {
    let e = fails(with_field(of(FieldType::Link, "to", Some("nowhere"))));
    assert!(has(&e, K::UnknownTarget, "thing", Some("to")));

    let mut child = simple("row", vec![]);
    child.is_child = true;
    let parent = simple("parent", vec![of(FieldType::Table, "rows", Some("row"))]);
    let linker = simple("linker", vec![of(FieldType::Link, "to", Some("row"))]);
    let e = fails(vec![child, parent, linker]);
    assert!(has(&e, K::UnknownTarget, "linker", Some("to")));
}

#[test]
fn tables_need_a_child_doctype_used_once() {
    let e = fails(vec![
        simple("master", vec![]),
        simple("p", vec![of(FieldType::Table, "rows", Some("master"))]),
    ]);
    assert!(has(&e, K::BadTable, "p", Some("rows")));

    let e = fails(with_field(of(FieldType::Table, "rows", Some("gone"))));
    assert!(has(&e, K::UnknownTarget, "thing", Some("rows")));

    let mut child = simple("row", vec![]);
    child.is_child = true;
    let p1 = simple("p1", vec![of(FieldType::Table, "rows", Some("row"))]);
    let p2 = simple("p2", vec![of(FieldType::Table, "rows", Some("row"))]);
    let e = fails(vec![child.clone(), p1, p2]);
    assert!(has(&e, K::BadTable, "row", None), "{e:?}");

    let e = fails(vec![child]);
    assert!(
        has(&e, K::BadTable, "row", None),
        "an unused child DocType is refused"
    );
}

#[test]
fn children_cannot_nest_or_be_posted() {
    let mut inner = simple("inner", vec![]);
    inner.is_child = true;
    let mut outer = simple(
        "outer",
        vec![of(FieldType::Table, "inner_rows", Some("inner"))],
    );
    outer.is_child = true;
    let top = simple(
        "top",
        vec![of(FieldType::Table, "outer_rows", Some("outer"))],
    );
    let e = fails(vec![inner, outer, top]);
    assert!(has(&e, K::BadTable, "outer", Some("inner_rows")));

    let mut posted_child = simple("pc", vec![]);
    posted_child.is_child = true;
    posted_child.is_posted = true;
    let top = simple("top", vec![of(FieldType::Table, "rows", Some("pc"))]);
    assert!(has(
        &fails(vec![posted_child, top]),
        K::BadTable,
        "pc",
        None
    ));
}

#[test]
fn title_field_must_exist_and_be_text_like() {
    let mut d = simple(
        "thing",
        vec![text("a"), FieldSpec::new("n", FieldType::Int)],
    );
    d.title_field = Some("missing".into());
    assert!(has(
        &fails(vec![d.clone()]),
        K::BadTitleField,
        "thing",
        None
    ));
    d.title_field = Some("n".into());
    assert!(has(
        &fails(vec![d.clone()]),
        K::BadTitleField,
        "thing",
        None
    ));
    d.title_field = Some("a".into());
    assert!(build(vec![d]).is_ok());
}

#[test]
fn scale_and_precision_are_checked() {
    let dec = |scale, precision| {
        let mut f = FieldSpec::new("amt", FieldType::Decimal);
        f.scale = scale;
        f.precision = precision;
        with_field(f)
    };
    assert!(has(
        &fails(dec(None, None)),
        K::BadProperty,
        "thing",
        Some("amt")
    ));
    assert!(has(
        &fails(dec(Some(29), Some(38))),
        K::BadProperty,
        "thing",
        Some("amt")
    ));
    assert!(has(
        &fails(dec(Some(20), None)),
        K::BadProperty,
        "thing",
        Some("amt")
    ));
    assert!(has(
        &fails(dec(Some(2), Some(39))),
        K::BadProperty,
        "thing",
        Some("amt")
    ));
    assert!(build(dec(Some(28), Some(38))).is_ok());
    assert!(build(dec(Some(0), None)).is_ok());

    let mut f = text("t");
    f.scale = Some(2);
    assert!(has(
        &fails(with_field(f)),
        K::BadProperty,
        "thing",
        Some("t")
    ));
}

#[test]
fn property_applicability_is_checked() {
    let mut f = FieldSpec::new("n", FieldType::Int);
    f.max_chars = Some(3);
    assert!(has(
        &fails(with_field(f)),
        K::BadProperty,
        "thing",
        Some("n")
    ));
    let mut f = text("t");
    f.options = Some("x".into());
    assert!(has(
        &fails(with_field(f)),
        K::BadProperty,
        "thing",
        Some("t")
    ));
    let mut f = text("t");
    f.max_chars = Some(0);
    assert!(has(
        &fails(with_field(f)),
        K::BadProperty,
        "thing",
        Some("t")
    ));
    let mut f = of(FieldType::Table, "rows", Some("row"));
    f.unique = true;
    let mut row = simple("row", vec![]);
    row.is_child = true;
    assert!(has(
        &fails(vec![row, simple("thing", vec![f])]),
        K::BadProperty,
        "thing",
        Some("rows")
    ));
}

#[test]
fn defaults_must_parse_for_their_type() {
    let ok = |ty, default: &str| {
        let mut f = FieldSpec::new("f", ty);
        f.default = Some(default.into());
        if ty == FieldType::Decimal {
            f.scale = Some(2);
        }
        if ty == FieldType::Select {
            f.options = Some("a\nb".into());
        }
        build(with_field(f)).is_ok()
    };
    for (ty, good, bad) in [
        (FieldType::Bool, "true", "yes"),
        (FieldType::Int, "-12", "1.5"),
        (FieldType::Decimal, "12.50", "12.505"),
        (FieldType::Decimal, "0.1", "abc"),
        (FieldType::Select, "b", "c"),
        (FieldType::Date, "2024-02-29", "2023-02-29"),
        (FieldType::Date, "1999-12-31", "2024-13-01"),
        (FieldType::Time, "23:59", "24:00"),
        (FieldType::Time, "08:30:15.250", "8:30"),
        (
            FieldType::DateTime,
            "2026-01-31T09:30:00Z",
            "2026-01-31 09:30:00",
        ),
        (
            FieldType::DateTime,
            "2026-01-31T09:30:00+05:00",
            "2026-01-31T09:30:00",
        ),
        (FieldType::Text, "anything", ""),
    ] {
        assert!(ok(ty, good), "{ty:?} {good}");
        if ty != FieldType::Text {
            assert!(!ok(ty, bad), "{ty:?} {bad}");
        }
    }
    let mut f = text("short");
    f.max_chars = Some(3);
    f.default = Some("abcd".into());
    assert!(has(
        &fails(with_field(f)),
        K::BadDefault,
        "thing",
        Some("short")
    ));
    let mut f = FieldSpec::new("j", FieldType::Json);
    f.default = Some("{}".into());
    assert!(has(
        &fails(with_field(f)),
        K::BadDefault,
        "thing",
        Some("j")
    ));
}

#[test]
fn select_needs_distinct_options() {
    let sel = |options: Option<&str>| with_field(of(FieldType::Select, "s", options));
    assert!(has(&fails(sel(None)), K::BadProperty, "thing", Some("s")));
    assert!(has(
        &fails(sel(Some(" \n "))),
        K::BadOptions,
        "thing",
        Some("s")
    ));
    assert!(has(
        &fails(sel(Some("a\nb\na"))),
        K::BadOptions,
        "thing",
        Some("s")
    ));
    assert!(build(sel(Some("a\nb"))).is_ok());
}

#[test]
fn dynamic_links_need_a_text_type_field() {
    let dl = |others: Vec<FieldSpec>, target: &str| {
        let mut fields = others;
        fields.push(of(FieldType::DynamicLink, "about", Some(target)));
        build(vec![simple("thing", fields)])
    };
    assert!(dl(vec![text("kind")], "kind").is_ok());
    let e = dl(vec![], "kind").err().unwrap_or_default();
    assert!(has(&e, K::UnknownTarget, "thing", Some("about")));
    let e = dl(vec![FieldSpec::new("kind", FieldType::Int)], "kind")
        .err()
        .unwrap_or_default();
    assert!(has(&e, K::BadProperty, "thing", Some("about")));
}

#[test]
fn index_fields_must_exist() {
    let mut d = simple("thing", vec![text("a")]);
    d.indexes = vec![
        IndexSpec {
            fields: vec!["a".into(), "ghost".into()],
            unique: false,
        },
        IndexSpec {
            fields: vec![],
            unique: true,
        },
    ];
    let e = fails(vec![d]);
    assert_eq!(kinds(&e), [K::BadIndex, K::BadIndex]);

    let mut posted = simple("p", vec![text("a")]);
    posted.is_posted = true;
    posted.indexes = vec![IndexSpec {
        fields: vec!["doc_state".into(), "a".into()],
        unique: false,
    }];
    assert!(
        build(vec![posted]).is_ok(),
        "engine fields can be indexed on posted DocTypes"
    );
}

#[test]
fn all_errors_are_reported_at_once() {
    let mut d = simple(
        "thing",
        vec![text("id"), of(FieldType::Link, "to", Some("x"))],
    );
    d.title_field = Some("zzz".into());
    let e = fails(vec![d]);
    assert_eq!(e.len(), 3, "{e:?}");
    assert!(e.iter().all(|e| e.doctype.as_deref() == Some("thing")));
    assert!(e.iter().all(|e| e.to_string().contains("`thing")));
}

#[test]
fn load_dir_reports_file_problems() {
    let dir = std::env::temp_dir().join(format!("spot-meta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("a.toml"), "name = \"b\"\n").unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("c.toml"), "name = \"c\"\nnope = 1\n")
        .unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("ignored.txt"), "x").unwrap_or_else(|e| panic!("{e}"));
    let e = spot_meta::load_dir(&dir).err().unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(kinds(&e), [K::FileName, K::Parse]);
    assert_eq!(e[1].file.as_deref(), Some("c.toml"));
    assert!(e[1].message.contains("nope"), "{}", e[1].message);

    let missing = spot_meta::load_dir(&dir).err().unwrap_or_default();
    assert_eq!(kinds(&missing), [K::Io]);
}

#[test]
fn fixtures_can_be_mutated_into_failure() {
    let mut defs = fixtures();
    for d in &mut defs {
        if d.name == "invoice" {
            d.fields.retain(|f| f.name != "lines");
        }
    }
    let e = fails(defs);
    assert!(has(&e, K::BadTable, "invoice_line", None));
}
