//! The ADR-087 constructs in metadata (WP-MET-05.T1): `geo` and `range` fields, the deprecated
//! `geolocation` alias, DocType-level `exclusions` and `search`, their validations and the
//! backend-capability refusal. Pure tests: no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{build, kinds, simple, text};
use spot_meta::{
    Definition, EffectiveMeta, ExclusionSpec, FieldSpec, FieldType, MetaError, MetaErrorKind as K,
    SearchFieldSpec, SearchSpec, formats::check_geolocation_point,
};
use spot_storage_contract::schema::{
    Catalog, Collation, FieldType as Storage, RangeOf, SearchWeight,
};
use spot_storage_contract::{Capabilities, GEO_SRID, MAX_SEARCH_FIELDS, Name};

fn en() -> Collation {
    Collation::new("en").unwrap()
}

fn typed(name: &str, ty: FieldType, format: Option<&str>) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.format = format.map(str::to_owned);
    f
}

fn required(mut f: FieldSpec) -> FieldSpec {
    f.required = true;
    f
}

fn range(name: &str, format: &str) -> FieldSpec {
    typed(name, FieldType::Range, Some(format))
}

fn booking() -> Definition {
    let mut d = simple(
        "booking",
        vec![
            required(text("unit")),
            range("stay", "date_range"),
            typed("spot", FieldType::Geo, Some("point")),
            text("guest"),
            FieldSpec::new("notes", FieldType::LongText),
        ],
    );
    d.exclusions = vec![ExclusionSpec {
        field: "stay".into(),
        scope: vec!["unit".into()],
        message: "That unit is already booked for those dates.".into(),
    }];
    d.search = Some(SearchSpec {
        language: Some("en".into()),
        fields: vec![
            SearchFieldSpec {
                name: "guest".into(),
                weight: "A".into(),
            },
            SearchFieldSpec {
                name: "notes".into(),
                weight: "D".into(),
            },
        ],
        facets: vec!["unit".into()],
        typo_tolerance: true,
    });
    d
}

fn built(d: Definition) -> EffectiveMeta {
    build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"))
}

fn errors(d: Definition) -> Vec<MetaError> {
    build(vec![d]).expect_err("the build must be refused")
}

fn catalog(d: Definition) -> Catalog {
    built(d).catalog(en()).unwrap()
}

fn caps(range_exclusion: bool, search: bool) -> Capabilities {
    Capabilities {
        backend: "memory".into(),
        contract_version: "0.2.0".into(),
        read_sources: vec![],
        max_writes_per_tx: 100,
        feed_retention: Duration::from_secs(60),
        geo_filters: false,
        range_exclusion,
        search,
    }
}

// --- the happy path ----------------------------------------------------------------------

#[test]
fn a_doctype_using_every_construct_compiles_to_the_expected_catalog() {
    let cat = catalog(booking());
    let e = cat.entity(&Name::new("booking").unwrap()).unwrap();
    let ty = |n: &str| {
        e.fields
            .iter()
            .find(|f| f.name.as_str() == n)
            .unwrap()
            .ty
            .clone()
    };
    assert_eq!(ty("spot"), Storage::Geo { srid: GEO_SRID });
    assert_eq!(ty("stay"), Storage::Range { of: RangeOf::Date });

    assert_eq!(e.exclusions.len(), 1);
    let x = &e.exclusions[0];
    assert_eq!(x.field.as_str(), "stay");
    assert_eq!(
        x.scope.iter().map(Name::as_str).collect::<Vec<_>>(),
        ["unit"]
    );
    assert_eq!(x.name.as_str(), "ex_booking_stay");
    assert_eq!(x.message, "That unit is already booked for those dates.");

    let s = e.search.as_ref().unwrap();
    assert_eq!(s.language.as_deref(), Some("en"));
    assert_eq!(s.fields.len(), 2);
    assert_eq!(s.fields[0].weight, SearchWeight::A);
    assert_eq!(s.fields[1].weight, SearchWeight::D);
    assert_eq!(s.facets[0].as_str(), "unit");
    assert!(s.typo_tolerance);
}

#[test]
fn an_instant_range_compiles_to_a_datetime_range() {
    let cat = catalog(simple("shift", vec![range("worked", "instant_range")]));
    let e = cat.entity(&Name::new("shift").unwrap()).unwrap();
    assert_eq!(
        e.fields[0].ty,
        Storage::Range {
            of: RangeOf::DateTime
        }
    );
}

#[test]
fn a_doctype_without_the_constructs_compiles_to_none_of_them() {
    let cat = catalog(simple("note", vec![text("body")]));
    let e = cat.entity(&Name::new("note").unwrap()).unwrap();
    assert!(e.exclusions.is_empty());
    assert!(e.search.is_none());
}

#[test]
fn the_declarations_round_trip_through_toml() {
    let d = booking();
    let text = toml::to_string(&d).unwrap();
    let back: Definition = toml::from_str(&text).unwrap();
    assert_eq!(back, d);
}

#[test]
fn the_app_view_describes_geo_and_range_as_json_with_a_warning() {
    let m = built(booking());
    let n = m
        .warnings()
        .iter()
        .filter(|w| w.code == "meta.app_kind_approximated")
        .count();
    assert_eq!(n, 2);
}

// --- formats -----------------------------------------------------------------------------

#[test]
fn geo_and_range_need_their_formats() {
    let k = |f: FieldSpec| kinds(&errors(simple("a", vec![f])));
    assert_eq!(k(typed("p", FieldType::Geo, Some("polygon"))).len(), 1);
    let e = errors(simple("a", vec![FieldSpec::new("r", FieldType::Range)]));
    assert_eq!(e.len(), 1);
    assert!(e[0].message.contains("needs a `format`"));
    assert_eq!(k(range("r", "int_range")).len(), 1);
}

#[test]
fn a_geo_or_range_field_cannot_be_unique_or_btree_indexed() {
    let mut f = range("stay", "date_range");
    f.unique = true;
    assert!(!errors(simple("a", vec![f])).is_empty());
    let mut g = typed("spot", FieldType::Geo, Some("point"));
    g.index = true;
    assert!(!errors(simple("a", vec![g])).is_empty());
    let mut d = simple("a", vec![range("stay", "date_range")]);
    d.indexes = vec![spot_meta::IndexSpec {
        fields: vec!["stay".into()],
        unique: false,
    }];
    assert!(kinds(&errors(d)).contains(&K::BadIndex));
}

#[test]
fn a_geo_field_has_no_default() {
    let mut f = typed("spot", FieldType::Geo, Some("point"));
    f.default = Some("POINT(0 0)".into());
    assert!(!errors(simple("a", vec![f])).is_empty());
}

// --- range defaults ----------------------------------------------------------------------

fn with_default(format: &str, default: &str) -> Definition {
    let mut f = range("stay", format);
    f.default = Some(default.into());
    simple("a", vec![f])
}

#[test]
fn a_finite_half_open_range_default_is_accepted() {
    built(with_default("date_range", "[2026-01-01,2026-01-08)"));
    built(with_default(
        "instant_range",
        "[2026-01-01T10:00:00Z,2026-01-01T11:00:00Z)",
    ));
}

#[test]
fn empty_unbounded_and_wrongly_bracketed_defaults_are_refused_with_bad_default() {
    for (format, v) in [
        ("date_range", "[2026-01-08,2026-01-01)"),
        ("date_range", "[2026-01-01,2026-01-01)"),
        ("date_range", "[2026-01-01,)"),
        ("date_range", "[,2026-01-01)"),
        ("date_range", "[2026-01-01,infinity)"),
        ("date_range", "(2026-01-01,2026-01-08)"),
        ("date_range", "[2026-01-01,2026-01-08]"),
        ("date_range", "[2026-01-01]"),
        ("date_range", "[2026-01-01,tomorrow)"),
        ("instant_range", "[2026-01-01,2026-01-02)"),
        (
            "instant_range",
            "[2026-01-01T11:00:00Z,2026-01-01T10:00:00Z)",
        ),
    ] {
        let e = errors(with_default(format, v));
        assert_eq!(kinds(&e), [K::BadDefault], "{v}");
    }
}

// --- the alias ---------------------------------------------------------------------------

#[test]
fn the_geolocation_alias_still_compiles_and_is_flagged_deprecated() {
    let m = built(simple(
        "place",
        vec![typed("where", FieldType::Json, Some("geolocation"))],
    ));
    assert!(
        m.warnings()
            .iter()
            .any(|w| w.code == "meta.geolocation_deprecated")
    );
    let cat = m.catalog(en()).unwrap();
    let e = cat.entity(&Name::new("place").unwrap()).unwrap();
    assert_eq!(e.fields[0].ty, Storage::Json);
}

#[test]
fn a_single_point_can_move_to_geo() {
    let p = check_geolocation_point(r#"{"type":"Point","coordinates":[73.0479,33.6844]}"#).unwrap();
    assert_eq!(p.validate(), Ok(()));
}

#[test]
fn anything_but_a_single_point_is_refused() {
    for v in [
        r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}"#,
        r#"{"type":"FeatureCollection","features":[]}"#,
        r#"{"type":"Point","coordinates":[1,2,3]}"#,
        r#"{"type":"Point","coordinates":[181,0]}"#,
        r#"{"type":"Point","coordinates":[0,91]}"#,
        r#"{"type":"Point","coordinates":["a","b"]}"#,
        r#"{"coordinates":[1,2]}"#,
        "not json",
    ] {
        assert!(check_geolocation_point(v).is_err(), "{v}");
    }
}

// --- exclusions --------------------------------------------------------------------------

fn bad_exclusion(edit: impl FnOnce(&mut Definition)) -> Vec<MetaError> {
    let mut d = booking();
    edit(&mut d);
    errors(d)
}

#[test]
fn an_exclusion_naming_a_missing_field_is_refused() {
    let e = bad_exclusion(|d| d.exclusions[0].field = "gone".into());
    assert_eq!(kinds(&e), [K::BadExclusion]);
    let e = bad_exclusion(|d| d.exclusions[0].scope = vec!["gone".into()]);
    assert_eq!(kinds(&e), [K::BadExclusion]);
}

#[test]
fn an_exclusion_over_a_non_range_field_is_refused() {
    let e = bad_exclusion(|d| d.exclusions[0].field = "guest".into());
    assert_eq!(kinds(&e), [K::BadExclusion]);
}

#[test]
fn a_nullable_scope_field_is_refused() {
    let e = bad_exclusion(|d| d.exclusions[0].scope = vec!["guest".into()]);
    assert_eq!(kinds(&e), [K::BadExclusion]);
    assert!(e[0].message.contains("not `required`"));
}

#[test]
fn a_non_scalar_scope_field_is_refused() {
    let e = bad_exclusion(|d| {
        d.fields
            .push(required(typed("area", FieldType::Geo, Some("point"))));
        d.exclusions[0].scope = vec!["area".into()];
    });
    assert_eq!(kinds(&e), [K::BadExclusion]);
}

#[test]
fn a_repeated_exclusion_or_scope_and_an_empty_message_are_refused() {
    let e = bad_exclusion(|d| d.exclusions.push(d.exclusions[0].clone()));
    assert_eq!(kinds(&e), [K::BadExclusion]);
    let e = bad_exclusion(|d| d.exclusions[0].scope = vec!["unit".into(), "unit".into()]);
    assert_eq!(kinds(&e), [K::BadExclusion]);
    let e = bad_exclusion(|d| d.exclusions[0].message = "  ".into());
    assert_eq!(kinds(&e), [K::BadExclusion]);
}

#[test]
fn an_exclusion_with_no_scope_is_allowed() {
    let mut d = booking();
    d.exclusions[0].scope.clear();
    built(d);
}

// --- search ------------------------------------------------------------------------------

fn bad_search(edit: impl FnOnce(&mut SearchSpec, &mut Vec<FieldSpec>)) -> Vec<MetaError> {
    let mut d = booking();
    let mut s = d.search.take().unwrap();
    edit(&mut s, &mut d.fields);
    d.search = Some(s);
    errors(d)
}

fn sf(name: &str, weight: &str) -> SearchFieldSpec {
    SearchFieldSpec {
        name: name.into(),
        weight: weight.into(),
    }
}

#[test]
fn a_missing_search_field_is_refused() {
    let e = bad_search(|s, _| s.fields.push(sf("gone", "B")));
    assert_eq!(kinds(&e), [K::BadSearch]);
}

#[test]
fn a_search_field_that_is_not_text_is_refused() {
    let e = bad_search(|s, f| {
        f.push(FieldSpec::new("count", FieldType::Int));
        s.fields.push(sf("count", "B"));
    });
    assert_eq!(kinds(&e), [K::BadSearch]);
}

#[test]
fn a_weight_outside_a_to_d_is_refused() {
    for w in ["E", "a", "", "AA"] {
        let e = bad_search(|s, _| s.fields[0].weight = w.into());
        assert_eq!(kinds(&e), [K::BadSearch], "{w:?}");
    }
}

#[test]
fn too_many_empty_or_repeated_search_fields_are_refused() {
    let e = bad_search(|s, f| {
        s.fields.clear();
        for i in 0..=MAX_SEARCH_FIELDS {
            f.push(text(&format!("t{i}")));
            s.fields.push(sf(&format!("t{i}"), "C"));
        }
    });
    assert_eq!(kinds(&e), [K::BadSearch]);
    let e = bad_search(|s, _| s.fields.clear());
    assert_eq!(kinds(&e), [K::BadSearch]);
    let e = bad_search(|s, _| s.fields.push(sf("guest", "B")));
    assert_eq!(kinds(&e), [K::BadSearch]);
}

#[test]
fn exactly_the_maximum_number_of_search_fields_is_accepted() {
    let mut d = simple("wide", vec![]);
    let mut s = SearchSpec::default();
    for i in 0..MAX_SEARCH_FIELDS {
        d.fields.push(text(&format!("t{i}")));
        s.fields.push(sf(&format!("t{i}"), "B"));
    }
    d.search = Some(s);
    built(d);
}

#[test]
fn a_bad_language_and_a_bad_facet_are_refused() {
    let e = bad_search(|s, _| s.language = Some("en us!".into()));
    assert_eq!(kinds(&e), [K::BadSearch]);
    let e = bad_search(|s, _| s.facets = vec!["gone".into()]);
    assert_eq!(kinds(&e), [K::BadSearch]);
}

#[test]
fn a_protected_search_field_is_refused_with_its_own_code() {
    let e = bad_search(|_, f| {
        f.iter_mut()
            .find(|x| x.name == "guest")
            .unwrap()
            .protection_group = Some("pii".into());
    });
    // The undeclared group is reported too; the search rule is what this test is about.
    assert!(kinds(&e).contains(&K::SearchFieldProtected));
}

#[test]
fn a_protected_facet_is_refused() {
    let mut d = booking();
    d.fields
        .iter_mut()
        .find(|x| x.name == "unit")
        .unwrap()
        .protection_group = Some("pii".into());
    d.protection_groups = vec![spot_meta::ProtectionGroupSpec::new("pii", "Personal")];
    assert_eq!(kinds(&errors(d)), [K::SearchFieldProtected]);
}

#[test]
fn a_search_field_in_a_declared_protection_group_is_refused_and_unprotected_ones_pass() {
    let mut d = booking();
    d.fields
        .iter_mut()
        .find(|x| x.name == "guest")
        .unwrap()
        .protection_group = Some("pii".into());
    d.protection_groups = vec![spot_meta::ProtectionGroupSpec::new("pii", "Personal")];
    assert_eq!(kinds(&errors(d)), [K::SearchFieldProtected]);
}

// --- the backend refusal -----------------------------------------------------------------

#[test]
fn a_declared_exclusion_against_a_backend_without_it_is_refused_at_compile_time() {
    let m = built(booking());
    let e = m.catalog_for(en(), &caps(false, true)).unwrap_err();
    assert_eq!(e.kind, K::BackendUnsupported);
    assert_eq!(e.doctype.as_deref(), Some("booking"));
}

#[test]
fn a_declared_search_against_a_backend_without_it_is_refused_at_compile_time() {
    let m = built(booking());
    let e = m.catalog_for(en(), &caps(true, false)).unwrap_err();
    assert_eq!(e.kind, K::BackendUnsupported);
}

#[test]
fn a_backend_with_the_capabilities_compiles_the_same_catalog() {
    let m = built(booking());
    assert_eq!(
        m.catalog_for(en(), &caps(true, true)).unwrap(),
        m.catalog(en()).unwrap()
    );
}

#[test]
fn geo_storage_alone_is_never_refused_by_the_backend_check() {
    let m = built(simple(
        "place",
        vec![typed("spot", FieldType::Geo, Some("point"))],
    ));
    m.catalog_for(en(), &caps(false, false)).unwrap();
}

#[test]
fn warning_codes_in_compile_rs_are_plain_literals() {
    let src = include_str!("../src/compile.rs");
    let concat_call = ["concat", "!("].concat();
    let workaround = format!("{concat_call}\"meta\"");
    assert!(
        !src.contains(&workaround),
        "the warning codes must be plain literals so the error-codes lint sees them"
    );
    for code in [
        "\"meta.app_kind_approximated\"",
        "\"meta.geolocation_deprecated\"",
    ] {
        assert_eq!(
            src.matches(code).count(),
            1,
            "{code} must appear exactly once"
        );
    }
}
