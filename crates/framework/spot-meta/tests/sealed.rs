//! A `secret` field's storage path (WP-SEC-03.T2, ADR-068 §2): the sealed column, the
//! blind-index column and its index, and every rule that keeps a value out of everything else.
#![allow(missing_docs, clippy::expect_used)]

mod common;

use common::{build, kinds, simple};
use spot_app_api::meta::FieldKind;
use spot_meta::{
    Definition, FieldLookup, FieldSpec, FieldType, LookupSpec, MetaErrorKind as K, Registry,
    blind_index_column, sealed_column,
};
use spot_storage_contract::schema::{Collation, FieldType as Storage};

fn en() -> Collation {
    Collation::new("en").unwrap_or_else(|e| panic!("{e}"))
}

fn secret(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Secret)
}

fn lookup_secret(name: &str) -> FieldSpec {
    let mut f = secret(name);
    f.lookup = Some(FieldLookup::Index(true));
    f
}

fn catalog_of(fields: Vec<FieldSpec>) -> spot_storage_contract::schema::Catalog {
    let meta = build(vec![simple("doc", fields)]).unwrap_or_else(|e| panic!("{e:?}"));
    meta.catalog(en()).unwrap_or_else(|e| panic!("{e}"))
}

fn entity(fields: Vec<FieldSpec>) -> spot_storage_contract::schema::EntityDef {
    let cat = catalog_of(fields);
    cat.entity(&spot_app_api::prelude::Name::new("doc").expect("name"))
        .unwrap_or_else(|e| panic!("{e}"))
        .clone()
}

fn names(fields: Vec<FieldSpec>) -> Vec<String> {
    entity(fields)
        .fields
        .into_iter()
        .map(|f| f.name.to_string())
        .collect()
}

fn errors(fields: Vec<FieldSpec>) -> Vec<spot_meta::MetaErrorKind> {
    match build(vec![simple("doc", fields)]) {
        Ok(_) => Vec::new(),
        Err(e) => kinds(&e),
    }
}

#[test]
fn a_secret_compiles_to_a_nullable_bytes_sealed_column() {
    let e = entity(vec![secret("password")]);
    let column_name = sealed_column("password");
    let column = e
        .field(&spot_app_api::prelude::Name::new(&column_name).expect("name"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(column.ty, Storage::Bytes);
    assert!(column.nullable, "a secret may be unset");
    // The field itself has no column, and there is no index at all.
    assert!(names(vec![secret("password")]).contains(&"password__sealed".to_owned()));
    assert!(!names(vec![secret("password")]).contains(&"password".to_owned()));
    assert!(e.indexes.is_empty());
}

#[test]
fn a_lookup_secret_adds_a_blind_index_column_and_one_index() {
    let e = entity(vec![lookup_secret("token_digest")]);
    let bare = spot_app_api::prelude::Name::new("token_digest").expect("name");
    let sealed_name = sealed_column("token_digest");
    let bidx_name = blind_index_column("token_digest");
    let sealed = spot_app_api::prelude::Name::new(&sealed_name).expect("name");
    let bidx = spot_app_api::prelude::Name::new(&bidx_name).expect("name");
    assert!(e.field(&bare).is_err(), "no plain column");
    assert_eq!(e.field(&sealed).expect("sealed").ty, Storage::Bytes);
    assert_eq!(e.field(&bidx).expect("bidx").ty, Storage::Bytes);
    assert!(e.field(&bidx).expect("bidx").nullable);
    assert_eq!(e.indexes.len(), 1);
    let ix = &e.indexes[0];
    assert_eq!(ix.name.as_str(), "idx_doc_token_digest_bidx");
    assert_eq!(ix.fields, vec![bidx]);
    assert!(!ix.unique);
}

#[test]
fn a_unique_lookup_secret_indexes_uniquely() {
    let mut f = lookup_secret("token_digest");
    f.unique = true;
    let e = entity(vec![f]);
    assert_eq!(e.indexes.len(), 1);
    assert!(
        e.indexes[0].unique,
        "unique on the field means unique on the index"
    );
    assert_eq!(e.indexes[0].name.as_str(), "idx_doc_token_digest_bidx");
}

#[test]
fn the_app_view_is_unchanged_by_the_storage_path() {
    // Same field, same kind, same label: `lookup = true` describes storage, not the API.
    let plain = build(vec![simple("doc", vec![secret("password")])]).expect("build");
    let indexed = build(vec![simple("doc", vec![lookup_secret("password")])]).expect("build");
    let a = plain.doctype("doc").expect("compiled");
    let b = indexed.doctype("doc").expect("compiled");
    assert_eq!(a, b, "the app description is identical either way");
    let field = a.field("password").expect("the secret is described");
    assert_eq!(field.kind, FieldKind::Secret);
    assert!(
        field.lookup.is_none(),
        "a secret has no lookup in the app view"
    );
    assert!(
        a.fields.iter().all(|f| !f.name.as_str().contains("__")),
        "engine-managed columns never reach the app view"
    );
}

#[test]
fn a_field_naming_the_reserved_infix_is_refused() {
    for name in ["token__sealed", "token__bidx", "a__b"] {
        let e = errors(vec![secret(name)]);
        assert_eq!(e, vec![K::ReservedName], "{name}");
        let text = errors(vec![FieldSpec::new(name, FieldType::Text)]);
        assert_eq!(
            text,
            vec![K::ReservedName],
            "{name} is reserved on every type"
        );
    }
}

#[test]
fn a_secret_unique_without_lookup_is_refused() {
    let mut f = secret("password");
    f.unique = true;
    assert_eq!(errors(vec![f]), vec![K::BadProperty]);
}

#[test]
fn a_secret_cannot_be_indexed_or_shown_or_filtered() {
    for mutate in [
        (|f: &mut FieldSpec| f.index = true) as fn(&mut FieldSpec),
        |f: &mut FieldSpec| f.in_list_view = true,
        |f: &mut FieldSpec| f.in_standard_filter = true,
    ] {
        let mut f = secret("password");
        mutate(&mut f);
        assert_eq!(errors(vec![f]), vec![K::BadProperty]);
    }
}

#[test]
fn a_secret_cannot_have_a_default_or_be_fetched() {
    let mut f = secret("password");
    f.default = Some("hunter2".into());
    assert_eq!(errors(vec![f]), vec![K::BadDefault]);

    let mut g = secret("password");
    g.fetch_from = Some("owner.name".into());
    let mut link = FieldSpec::new("owner", FieldType::Link);
    link.options = Some("doc".into());
    assert_eq!(errors(vec![g, link]), vec![K::BadProperty]);
}

#[test]
fn the_lookup_flag_is_only_for_secrets() {
    for ty in [
        FieldType::Text,
        FieldType::Link,
        FieldType::Select,
        FieldType::Json,
        FieldType::Bytes,
    ] {
        let mut f = FieldSpec::new("thing", ty);
        f.options = (ty == FieldType::Link || ty == FieldType::Select).then(|| "doc".to_owned());
        f.lookup = Some(FieldLookup::Index(true));
        assert_eq!(errors(vec![f]), vec![K::BadProperty], "{ty:?}");
    }
}

#[test]
fn a_secret_takes_the_flag_and_not_a_lookup_table() {
    let mut f = secret("password");
    f.lookup = Some(FieldLookup::Spec(LookupSpec::seam("platform:link_search")));
    assert_eq!(errors(vec![f]), vec![K::BadProperty]);
}

#[test]
fn the_flag_parses_from_toml_and_survives_a_round_trip() {
    let text = r#"
name = "session"
[[fields]]
name = "token_digest"
type = "secret"
lookup = true
"#;
    let d = Definition::parse("session.toml", text).unwrap_or_else(|e| panic!("{e}"));
    assert!(d.fields[0].lookup_index());
    assert!(d.fields[0].lookup_spec().is_none());
    let again = Definition::parse(
        "session.toml",
        &d.to_toml().unwrap_or_else(|e| panic!("{e}")),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(again.fields[0].lookup, d.fields[0].lookup);
    // `lookup = false` means nothing at all.
    let off = text.replace("lookup = true", "lookup = false");
    let d = Definition::parse("session.toml", &off).unwrap_or_else(|e| panic!("{e}"));
    assert!(!d.fields[0].lookup_index());
}

#[test]
fn compiling_twice_gives_the_same_catalog() {
    let fields = vec![secret("password"), lookup_secret("token_digest")];
    let meta = build(vec![simple("doc", fields)]).unwrap_or_else(|e| panic!("{e:?}"));
    let repo = Registry::build(
        meta.definitions().to_vec(),
        &spot_meta::Customisations::new(),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(meta.catalog(en()).ok(), repo.catalog(en()).ok());
}
