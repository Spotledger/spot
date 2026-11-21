//! ADR-068 §2 types and formats: the sample DocType, its two compiled views, every validation
//! error and every text-format check.
#![allow(missing_docs)]

mod common;

use std::path::PathBuf;

use common::{build, kinds, of, simple, text};
use spot_app_api::meta::{
    AttachFormat, DecimalFormat, FieldKind, IntFormat, JsonFormat, LongTextFormat, TextFormat,
};
use spot_app_api::prelude::Decimal;
use spot_meta::formats::check_text_format;
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, MetaError, MetaErrorKind as K};
use spot_storage_contract::StorageBackend;
use spot_storage_contract::schema::{
    Collation, EntityKind, FieldType as Storage, LinkCheck, LinkTarget,
};
use spot_storage_contract::{Name, Value};

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn en() -> Collation {
    Collation::new("en").unwrap_or_else(|e| panic!("{e}"))
}

fn sample() -> Vec<Definition> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats");
    spot_meta::load_dir(&dir).unwrap_or_else(|e| panic!("format fixtures do not load: {e:?}"))
}

fn effective() -> EffectiveMeta {
    build(sample()).unwrap_or_else(|e| panic!("format fixtures do not build: {e:?}"))
}

fn demo() -> Definition {
    sample()
        .into_iter()
        .find(|d| d.name == "field_demo")
        .unwrap_or_else(|| panic!("field_demo"))
}

#[test]
fn the_sample_round_trips_through_toml() {
    for d in sample() {
        let text = d.to_toml().unwrap_or_else(|e| panic!("{e}"));
        let back = Definition::parse("round-trip", &text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, d, "{}", d.name);
    }
}

#[test]
fn the_sample_parses_to_the_expected_specs() {
    let d = demo();
    let f = |name: &str| {
        d.fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert_eq!(f("contact_email").ty, FieldType::Text);
    assert_eq!(f("contact_email").format.as_deref(), Some("email"));
    assert_eq!(f("ean").symbology.as_deref(), Some("ean13"));
    assert_eq!(f("script").syntax.as_deref(), Some("sql"));
    assert_eq!(f("score").max, Some(10));
    assert_eq!(f("stars").max, None);
    assert_eq!(f("price").currency_field.as_deref(), Some("currency"));
    assert_eq!(f("api_key").ty, FieldType::Secret);
    assert_eq!(f("document").ty, FieldType::Attach);
    assert_eq!(f("document").format, None);
    assert_eq!(f("tags").ty, FieldType::TableMultiselect);
    assert_eq!(f("tags").options.as_deref(), Some("demo_tag"));
    assert_eq!(f("notes").format, None);
}

#[test]
fn unknown_keys_are_still_refused() {
    let text = "name = \"t\"\n[[fields]]\nname = \"a\"\ntype = \"text\"\nformats = \"email\"\n";
    let err = Definition::parse("t.toml", text)
        .err()
        .map(|e| e.to_string());
    assert!(err.is_some_and(|e| e.contains("formats")));
}

#[test]
fn app_view_carries_the_formats() {
    let meta = effective();
    let v = meta
        .doctype("field_demo")
        .unwrap_or_else(|| panic!("field_demo"));
    let kind = |name: &str| {
        v.field(name)
            .unwrap_or_else(|| panic!("{name}"))
            .kind
            .clone()
    };
    let text = |format| FieldKind::Text {
        max_chars: None,
        format,
    };
    assert_eq!(kind("contact_email"), text(TextFormat::Email));
    assert_eq!(kind("contact_phone"), text(TextFormat::Phone));
    assert_eq!(kind("website"), text(TextFormat::Url));
    assert_eq!(kind("accent"), text(TextFormat::Color));
    assert_eq!(
        kind("ean"),
        text(TextFormat::Barcode {
            symbology: Some("ean13".into())
        })
    );
    assert_eq!(
        kind("notes"),
        FieldKind::LongText {
            format: LongTextFormat::Plain
        }
    );
    assert_eq!(
        kind("body_markdown"),
        FieldKind::LongText {
            format: LongTextFormat::Markdown
        }
    );
    assert_eq!(
        kind("body_html"),
        FieldKind::LongText {
            format: LongTextFormat::Html
        }
    );
    assert_eq!(
        kind("script"),
        FieldKind::LongText {
            format: LongTextFormat::Code {
                syntax: "sql".into()
            }
        }
    );
    assert_eq!(
        kind("lead_time"),
        FieldKind::Int {
            format: IntFormat::Duration
        }
    );
    assert_eq!(
        kind("score"),
        FieldKind::Int {
            format: IntFormat::Rating { max: 10 }
        }
    );
    assert_eq!(
        kind("stars"),
        FieldKind::Int {
            format: IntFormat::Rating { max: 5 }
        }
    );
    assert_eq!(
        kind("price"),
        FieldKind::Decimal {
            scale: 2,
            precision: 18,
            format: DecimalFormat::Currency {
                currency_field: n("currency")
            }
        }
    );
    assert_eq!(
        kind("discount"),
        FieldKind::Decimal {
            scale: 2,
            precision: 18,
            format: DecimalFormat::Percent
        }
    );
    assert_eq!(
        kind("place"),
        FieldKind::Json {
            format: JsonFormat::Geolocation
        }
    );
    assert_eq!(
        kind("extra"),
        FieldKind::Json {
            format: JsonFormat::Plain
        }
    );
    assert_eq!(
        kind("document"),
        FieldKind::Attach {
            format: AttachFormat::File
        }
    );
    assert_eq!(
        kind("photo"),
        FieldKind::Attach {
            format: AttachFormat::Image
        }
    );
    assert_eq!(
        kind("sign_off"),
        FieldKind::Attach {
            format: AttachFormat::Signature
        }
    );
    assert_eq!(kind("api_key"), FieldKind::Secret);
    assert_eq!(
        kind("tags"),
        FieldKind::TableMultiselect {
            doctype: n("demo_tag"),
            link_field: n("tag")
        }
    );
    assert_eq!(v.tables().get("tags"), Some(&&n("demo_tag")));
}

#[test]
fn app_view_defaults_are_parsed_and_a_secret_has_none() {
    let meta = effective();
    let v = meta
        .doctype("field_demo")
        .unwrap_or_else(|| panic!("field_demo"));
    let default = |name: &str| v.field(name).and_then(|f| f.default.clone());
    assert_eq!(
        default("contact_email"),
        Some(Value::Text("info@example.com".into()))
    );
    assert_eq!(default("lead_time"), Some(Value::Int(3600)));
    assert_eq!(default("score"), Some(Value::Int(7)));
    assert_eq!(
        default("discount"),
        Some(Value::Decimal(Decimal::new(1250, 2)))
    );
    for name in ["api_key", "document", "tags", "place"] {
        assert_eq!(default(name), None, "{name}");
    }
}

#[test]
fn catalog_keeps_the_storage_types_and_leaves_out_the_secret() {
    let meta = effective();
    let cat = meta.catalog(en()).unwrap_or_else(|e| panic!("{e}"));
    let demo = cat
        .entity(&n("field_demo"))
        .unwrap_or_else(|e| panic!("{e}"));
    let ty = |name: &str| {
        demo.field(&n(name))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .ty
            .clone()
    };
    for name in ["contact_email", "contact_phone", "website", "accent", "ean"] {
        assert!(matches!(ty(name), Storage::Text { .. }), "{name}");
    }
    assert_eq!(
        ty("code"),
        Storage::Text {
            max_chars: Some(20)
        }
    );
    for name in ["notes", "body_markdown", "body_html", "script"] {
        assert_eq!(ty(name), Storage::Text { max_chars: None }, "{name}");
    }
    for name in ["lead_time", "score", "stars"] {
        assert_eq!(ty(name), Storage::Int, "{name}");
    }
    for name in ["price", "discount"] {
        assert_eq!(
            ty(name),
            Storage::Decimal {
                precision: 18,
                scale: 2
            },
            "{name}"
        );
    }
    assert_eq!(ty("place"), Storage::Json);
    assert_eq!(ty("extra"), Storage::Json);
    for name in ["document", "photo", "sign_off"] {
        assert_eq!(
            ty(name),
            Storage::Link {
                target: LinkTarget::Entity(n("file")),
                check: LinkCheck::EngineOnly {
                    reason: "file records are engine-checked".into()
                },
            },
            "{name}"
        );
    }
    // A secret has no plain column; it has a sealed one beside the row (WP-SEC-03.T2).
    assert!(
        demo.field(&n("api_key")).is_err(),
        "a secret has no plain column"
    );
    assert_eq!(ty("api_key__sealed"), Storage::Bytes);
    assert!(
        demo.field(&n("api_key__bidx")).is_err(),
        "without `lookup = true` there is no blind index"
    );
    assert!(
        demo.field(&n("tags")).is_err(),
        "a multiselect is a child table"
    );
    assert_eq!(demo.children.len(), 1);
    assert_eq!(demo.children[0].field, n("tags"));
    assert_eq!(demo.children[0].entity, n("demo_tag"));
    assert!(
        demo.indexes
            .iter()
            .all(|i| !i.name.as_str().contains("api_key"))
    );

    let child = cat.entity(&n("demo_tag")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        child.kind,
        EntityKind::Child {
            parent: n("field_demo")
        }
    );
    assert!(child.field(&n("tag")).is_ok());
}

#[tokio::test]
async fn storage_accepts_the_sample_catalog() {
    let cat = effective().catalog(en()).unwrap_or_else(|e| panic!("{e}"));
    let backend = spot_storage_fake::FakeBackend::new();
    backend
        .prepare(&cat)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn compiling_the_sample_is_deterministic() {
    let (a, b) = (effective(), effective());
    assert_eq!(a.catalog(en()).ok(), b.catalog(en()).ok());
    let docs = |m: &EffectiveMeta| m.doctypes().map(|d| (**d).clone()).collect::<Vec<_>>();
    assert_eq!(docs(&a), docs(&b));
}

// ---- validation -----------------------------------------------------------------------------

/// The sample set with `field_demo` and its child replaced by a DocType holding `fields`, and
/// the other helper DocTypes (`file`, `currency`, `tag`) kept, so an error can only come from
/// the field under test.
fn with(fields: Vec<FieldSpec>) -> Vec<Definition> {
    let mut defs: Vec<_> = sample()
        .into_iter()
        .filter(|d| d.name != "field_demo" && d.name != "demo_tag")
        .collect();
    defs.push(simple("thing", fields));
    defs
}

fn errors_of(fields: Vec<FieldSpec>) -> Vec<MetaError> {
    build(with(fields))
        .err()
        .unwrap_or_else(|| panic!("expected validation errors"))
}

fn ok(fields: Vec<FieldSpec>) {
    if let Err(e) = build(with(fields)) {
        panic!("expected a valid set: {e:?}");
    }
}

fn fmt(ty: FieldType, format: &str) -> FieldSpec {
    let mut f = FieldSpec::new("f", ty);
    f.format = Some(format.into());
    f
}

fn only_error(fields: Vec<FieldSpec>, kind: K) -> MetaError {
    let errors = errors_of(fields);
    assert_eq!(errors.len(), 1, "{errors:?}");
    let e = errors[0].clone();
    assert_eq!(e.kind, kind, "{e}");
    assert_eq!(e.doctype.as_deref(), Some("thing"), "{e}");
    assert_eq!(e.field.as_deref(), Some("f"), "{e}");
    e
}

fn link_field() -> FieldSpec {
    of(FieldType::Link, "cur", Some("currency"))
}

#[test]
fn every_documented_format_is_accepted_on_its_base_type() {
    let cases: &[(FieldType, &str)] = &[
        (FieldType::Text, "plain"),
        (FieldType::Text, "email"),
        (FieldType::Text, "phone"),
        (FieldType::Text, "url"),
        (FieldType::Text, "color"),
        (FieldType::Text, "barcode"),
        (FieldType::LongText, "plain"),
        (FieldType::LongText, "markdown"),
        (FieldType::LongText, "html"),
        (FieldType::Int, "plain"),
        (FieldType::Int, "duration"),
        (FieldType::Int, "rating"),
        (FieldType::Json, "plain"),
        (FieldType::Json, "geolocation"),
    ];
    for (ty, format) in cases {
        ok(vec![fmt(*ty, format)]);
    }
    let mut dec = fmt(FieldType::Decimal, "plain");
    dec.scale = Some(2);
    ok(vec![dec.clone()]);
    dec.format = Some("percent".into());
    ok(vec![dec]);
    for format in ["file", "image", "signature"] {
        ok(vec![fmt(FieldType::Attach, format)]);
    }
    let mut code = fmt(FieldType::LongText, "code");
    code.syntax = Some("sql".into());
    ok(vec![code]);
}

#[test]
fn a_format_on_the_wrong_base_type_is_refused() {
    let mut dec = fmt(FieldType::Decimal, "email");
    dec.scale = Some(2);
    let wrong = [
        fmt(FieldType::Text, "markdown"),
        fmt(FieldType::LongText, "email"),
        fmt(FieldType::Int, "percent"),
        dec,
        fmt(FieldType::Json, "currency"),
        fmt(FieldType::Attach, "plain"),
        fmt(FieldType::Bool, "plain"),
        fmt(FieldType::Date, "plain"),
        fmt(FieldType::Secret, "plain"),
    ];
    for f in wrong {
        let what = format!("{:?} {:?}", f.ty, f.format);
        let e = only_error(vec![f], K::BadProperty);
        assert!(e.message.contains("format"), "{what}: {e}");
    }
}

#[test]
fn an_unknown_format_is_refused_and_lists_the_allowed_ones() {
    let e = only_error(vec![fmt(FieldType::Text, "iban")], K::BadProperty);
    assert!(
        e.message.contains("iban") && e.message.contains("email"),
        "{e}"
    );
}

#[test]
fn a_parameter_without_its_format_is_refused() {
    let mut a = text("f");
    a.syntax = Some("sql".into());
    let mut b = fmt(FieldType::Text, "email");
    b.symbology = Some("ean13".into());
    let mut c = FieldSpec::new("f", FieldType::Int);
    c.max = Some(5);
    let mut d = fmt(FieldType::Int, "duration");
    d.max = Some(5);
    let mut e = FieldSpec::new("f", FieldType::Decimal);
    e.scale = Some(2);
    e.currency_field = Some("cur".into());
    let mut g = fmt(FieldType::LongText, "markdown");
    g.syntax = Some("sql".into());
    for f in [a, b, c, d, e, g] {
        let what = format!("{:?} {:?}", f.ty, f.format);
        let mut fields = vec![f];
        fields.push(link_field());
        let errors = errors_of(fields);
        assert_eq!(errors.len(), 1, "{what}: {errors:?}");
        assert!(
            errors[0].message.contains("only for"),
            "{what}: {}",
            errors[0]
        );
    }
}

#[test]
fn required_parameters_are_checked() {
    let e = only_error(vec![fmt(FieldType::LongText, "code")], K::BadProperty);
    assert!(e.message.contains("syntax"), "{e}");
    let mut empty = fmt(FieldType::LongText, "code");
    empty.syntax = Some(String::new());
    only_error(vec![empty], K::BadProperty);

    let mut cur = fmt(FieldType::Decimal, "currency");
    cur.scale = Some(2);
    let e = only_error(vec![cur], K::BadProperty);
    assert!(e.message.contains("currency_field"), "{e}");
}

#[test]
fn currency_field_must_be_another_link_field_of_the_same_doctype() {
    let price = |target: &str| {
        let mut f = fmt(FieldType::Decimal, "currency");
        f.scale = Some(2);
        f.currency_field = Some(target.into());
        f
    };
    ok(vec![price("cur"), link_field()]);

    let e = only_error(vec![price("missing")], K::BadProperty);
    assert!(e.message.contains("does not exist"), "{e}");
    let e = only_error(vec![price("f")], K::BadProperty);
    assert!(e.message.contains("another field"), "{e}");
    let mut fields = vec![price("code")];
    fields.push(text("code"));
    let errors = errors_of(fields);
    assert!(
        errors
            .iter()
            .any(|e| e.field.as_deref() == Some("f") && e.message.contains("must be a link")),
        "{errors:?}"
    );
}

#[test]
fn rating_max_is_one_to_ten() {
    let rating = |max: u8| {
        let mut f = fmt(FieldType::Int, "rating");
        f.max = Some(max);
        f
    };
    for max in [1, 5, 10] {
        ok(vec![rating(max)]);
    }
    for max in [0, 11, 255] {
        let e = only_error(vec![rating(max)], K::BadProperty);
        assert!(e.message.contains("1 to 10"), "{e}");
    }
}

#[test]
fn defaults_must_satisfy_the_text_format_and_the_type() {
    let with_default = |format: &str, default: &str| {
        let mut f = fmt(FieldType::Text, format);
        f.default = Some(default.into());
        f
    };
    ok(vec![with_default("email", "a@b.example")]);
    only_error(vec![with_default("email", "nobody")], K::BadDefault);
    only_error(vec![with_default("phone", "12")], K::BadDefault);
    only_error(vec![with_default("url", "ftp://x")], K::BadDefault);
    only_error(vec![with_default("color", "red")], K::BadDefault);
    only_error(vec![with_default("barcode", "")], K::BadDefault);
    ok(vec![with_default("plain", "anything at all")]);

    let mut rating = fmt(FieldType::Int, "rating");
    rating.default = Some("6".into());
    only_error(vec![rating.clone()], K::BadDefault);
    rating.max = Some(10);
    ok(vec![rating]);
    let mut duration = fmt(FieldType::Int, "duration");
    duration.default = Some("soon".into());
    only_error(vec![duration], K::BadDefault);
    let mut pct = fmt(FieldType::Decimal, "percent");
    pct.scale = Some(1);
    pct.default = Some("12.55".into());
    only_error(vec![pct], K::BadDefault);
}

#[test]
fn a_secret_refuses_unique_index_and_default() {
    // Each is refused on its own: the kinds are BadProperty for unique and index, BadDefault.
    let secret = || FieldSpec::new("f", FieldType::Secret);
    ok(vec![secret()]);
    let mut unique = secret();
    unique.unique = true;
    let mut index = secret();
    index.index = true;
    let mut default = secret();
    default.default = Some("hunter2".into());
    for f in [unique, index, default] {
        let errors = errors_of(vec![f]);
        assert_eq!(errors.len(), 1, "{errors:?}");
    }
}

#[test]
fn a_secret_default_error_never_echoes_the_value() {
    let mut f = FieldSpec::new("f", FieldType::Secret);
    f.default = Some("hunter2".into());
    let errors = errors_of(vec![f]);
    assert!(
        errors.iter().all(|e| !e.to_string().contains("hunter2")),
        "{errors:?}"
    );
    assert!(errors.iter().any(|e| e.kind == K::BadDefault), "{errors:?}");
}

#[test]
fn a_secret_cannot_be_indexed_in_a_multi_field_index() {
    let mut defs = with(vec![FieldSpec::new("f", FieldType::Secret), text("g")]);
    if let Some(d) = defs.iter_mut().find(|d| d.name == "thing") {
        d.indexes.push(spot_meta::IndexSpec {
            fields: vec!["f".into(), "g".into()],
            unique: false,
        });
    }
    let errors = build(defs).err().unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&errors), [K::BadIndex]);
}

#[test]
fn attach_needs_the_file_doctype_one_error_per_field() {
    let mut defs = with(vec![
        FieldSpec::new("a", FieldType::Attach),
        FieldSpec::new("b", FieldType::Attach),
        text("c"),
    ]);
    defs.retain(|d| d.name != "file");
    let errors = build(defs).err().unwrap_or_else(|| panic!("must fail"));
    // The sample's own attach fields are gone with `field_demo`; only `thing` is left.
    assert_eq!(errors.len(), 2, "{errors:?}");
    for (e, field) in errors.iter().zip(["a", "b"]) {
        assert_eq!(e.kind, K::UnknownTarget);
        assert_eq!(e.doctype.as_deref(), Some("thing"));
        assert_eq!(e.field.as_deref(), Some(field));
        assert!(e.message.contains("file"), "{e}");
    }
    ok(vec![FieldSpec::new("a", FieldType::Attach)]);
}

#[test]
fn attach_and_secret_take_no_options_scale_or_max_chars() {
    for ty in [FieldType::Attach, FieldType::Secret] {
        let mut f = FieldSpec::new("f", ty);
        f.options = Some("file".into());
        only_error(vec![f], K::BadProperty);
        let mut f = FieldSpec::new("f", ty);
        f.max_chars = Some(5);
        only_error(vec![f], K::BadProperty);
    }
}

fn multiselect(child: &str) -> FieldSpec {
    of(FieldType::TableMultiselect, "f", Some(child))
}

fn child(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = simple(name, fields);
    d.is_child = true;
    d
}

#[test]
fn table_multiselect_needs_a_child_with_exactly_one_link_field() {
    let mut defs = with(vec![multiselect("demo_tag")]);
    defs.push(child(
        "demo_tag",
        vec![of(FieldType::Link, "tag", Some("tag"))],
    ));
    build(defs.clone()).unwrap_or_else(|e| panic!("{e:?}"));

    // No link field.
    if let Some(c) = defs.iter_mut().find(|d| d.name == "demo_tag") {
        c.fields = vec![text("label")];
    }
    let e = build(defs.clone())
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&e), [K::BadTable], "{e:?}");
    assert!(e[0].message.contains("exactly one link"), "{}", e[0]);

    // Two link fields.
    if let Some(c) = defs.iter_mut().find(|d| d.name == "demo_tag") {
        c.fields = vec![
            of(FieldType::Link, "tag", Some("tag")),
            of(FieldType::Link, "other", Some("tag")),
        ];
    }
    let e = build(defs).err().unwrap_or_else(|| panic!("must fail"));
    assert_eq!(kinds(&e), [K::BadTable], "{e:?}");
}

#[test]
fn table_multiselect_child_rules_cover_both_table_kinds() {
    let base = |fields: Vec<FieldSpec>| {
        let mut defs: Vec<_> = sample()
            .into_iter()
            .filter(|d| d.name != "field_demo" && d.name != "demo_tag")
            .collect();
        defs.push(simple("thing", fields));
        defs.push(child("rows", vec![of(FieldType::Link, "tag", Some("tag"))]));
        defs
    };
    // Used once: fine, as a plain table too.
    build(base(vec![multiselect("rows")])).unwrap_or_else(|e| panic!("{e:?}"));
    build(base(vec![of(FieldType::Table, "f", Some("rows"))])).unwrap_or_else(|e| panic!("{e:?}"));

    // Used by a table and a table_multiselect, or by two multiselects: refused.
    for second in [
        of(FieldType::Table, "g", Some("rows")),
        of(FieldType::TableMultiselect, "g", Some("rows")),
    ] {
        let e = build(base(vec![multiselect("rows"), second]))
            .err()
            .unwrap_or_else(|| panic!("must fail"));
        assert!(
            e.iter()
                .any(|e| e.doctype.as_deref() == Some("rows") && e.kind == K::BadTable),
            "{e:?}"
        );
    }

    // Not a child, unknown, nested in a child, unique/indexed.
    let e = build(base(vec![multiselect("tag")]))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert!(
        e.iter()
            .any(|e| e.kind == K::BadTable && e.field.as_deref() == Some("f")),
        "{e:?}"
    );
    let e = build(base(vec![multiselect("nowhere")]))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert!(e.iter().any(|e| e.kind == K::UnknownTarget), "{e:?}");
    let mut uniq = multiselect("rows");
    uniq.unique = true;
    let e = build(base(vec![uniq]))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert!(e.iter().any(|e| e.kind == K::BadProperty), "{e:?}");
    let mut defs = base(vec![]);
    if let Some(c) = defs.iter_mut().find(|d| d.name == "rows") {
        c.fields
            .push(of(FieldType::TableMultiselect, "inner", Some("rows")));
    }
    let e = build(defs).err().unwrap_or_else(|| panic!("must fail"));
    assert!(e.iter().any(|e| e.kind == K::BadTable), "{e:?}");
}

#[test]
fn all_format_errors_are_reported_at_once() {
    let mut bad_code = fmt(FieldType::LongText, "code");
    bad_code.name = "a".into();
    let mut bad_rating = fmt(FieldType::Int, "rating");
    bad_rating.name = "b".into();
    bad_rating.max = Some(0);
    let mut bad_default = fmt(FieldType::Text, "email");
    bad_default.name = "c".into();
    bad_default.default = Some("x".into());
    let mut unknown = fmt(FieldType::Json, "nope");
    unknown.name = "d".into();
    let errors = errors_of(vec![bad_code, bad_rating, bad_default, unknown]);
    let fields: Vec<_> = errors.iter().filter_map(|e| e.field.as_deref()).collect();
    assert_eq!(fields, ["a", "b", "c", "d"], "{errors:?}");
    assert!(errors.iter().all(|e| e.doctype.as_deref() == Some("thing")));
}

// ---- text format checks -----------------------------------------------------------------------

fn accepts(format: &str, values: &[&str]) {
    for v in values {
        assert!(
            check_text_format(format, v).is_ok(),
            "{format}: `{v}` should pass"
        );
    }
}

fn rejects(format: &str, values: &[&str]) {
    for v in values {
        let r = check_text_format(format, v);
        assert!(r.is_err(), "{format}: `{v}` should fail");
    }
}

#[test]
fn email_check() {
    accepts(
        "email",
        &["a@b.co", "first.last+tag@mail.example.org", "x@y.z"],
    );
    rejects(
        "email",
        &[
            "", "plain", "@b.co", "a@", "a@b", "a@@b.co", "a@b@c.co", "a b@c.co", "a@b .co",
            "a@.co", "a@b.", "a@b..co",
        ],
    );
    let local = "a".repeat(250);
    rejects("email", &[&format!("{local}@b.co")]);
    let local = "a".repeat(247);
    accepts("email", &[&format!("{local}@b.co")]);
}

#[test]
fn phone_check() {
    accepts(
        "phone",
        &[
            "+49 30 1234567",
            "(030) 123-4567",
            "12345",
            "+1-555-010-1234",
        ],
    );
    rejects(
        "phone",
        &[
            "",
            "1234",
            "+1 23",
            "abcde",
            "12345x",
            "++1234567",
            "1+2345678",
            "123456789012345678901",
            "- - - - -",
        ],
    );
}

#[test]
fn url_check() {
    accepts(
        "url",
        &[
            "http://a",
            "https://example.com/path?q=1#x",
            "mailto:a@b.co",
        ],
    );
    rejects(
        "url",
        &[
            "",
            "example.com",
            "ftp://x",
            "http://",
            "https://",
            "mailto:",
            "http://a b",
            "javascript:alert(1)",
            "HTTP://X",
        ],
    );
}

#[test]
fn color_check() {
    accepts("color", &["#fff", "#FFF", "#1a2B3c", "#11223344"]);
    rejects(
        "color",
        &[
            "",
            "#",
            "fff",
            "#ff",
            "#ffff",
            "#fffff",
            "#fffffff",
            "#ggg",
            "#123456789",
            "red",
        ],
    );
}

#[test]
fn barcode_check() {
    accepts(
        "barcode",
        &["1", "4006381333931", "ABC-123 x", &"9".repeat(64)],
    );
    rejects(
        "barcode",
        &["", &"9".repeat(65), "tab\there", "naïve", "line\nbreak"],
    );
}

#[test]
fn plain_passes_anything_and_other_names_are_refused() {
    accepts("plain", &["", "anything"]);
    rejects("markdown", &["x"]);
    rejects("html", &["x"]);
    rejects("rating", &["x"]);
    rejects("nope", &["x"]);
}
