//! The reverse-index compiler (WP-STO-07.T6/T8) through the public API of `spot-meta`.
#![allow(missing_docs, clippy::expect_used)]

mod common;

use common::{build, of, simple, text};
use spot_app_api::prelude::Name;
use spot_meta::{Definition, EffectiveMeta, FieldType, MetaErrorKind, ReverseIndex};
use spot_storage_contract::IndexMethod;
use spot_storage_contract::schema::{Catalog, Collation};

fn en() -> Collation {
    Collation::new("en").expect("collation")
}

fn defs(index_true: bool) -> Vec<Definition> {
    let mut owner = of(FieldType::Link, "owner", Some("node"));
    owner.index = index_true;
    vec![
        simple("edge", vec![owner, text("note")]),
        simple("node", vec![text("title")]),
    ]
}

fn meta(index_true: bool) -> EffectiveMeta {
    build(defs(index_true)).expect("meta builds")
}

fn ri(table: &str, column: &str, name: &str) -> ReverseIndex {
    ReverseIndex {
        table: table.into(),
        column: column.into(),
        name: name.into(),
    }
}

fn index_names(cat: &Catalog, table: &str) -> Vec<String> {
    cat.entity(&Name::new(table).expect("name"))
        .expect("entity")
        .indexes
        .iter()
        .map(|i| i.name.as_str().to_owned())
        .collect()
}

#[test]
fn an_entry_becomes_one_plain_single_column_btree_index() {
    let cat = meta(false)
        .catalog_with_reverse_indexes(en(), &[ri("edge", "owner", "gx_edge_owner_rev")])
        .expect("catalog");
    let edge = cat
        .entity(&Name::new("edge").expect("name"))
        .expect("entity");
    assert_eq!(edge.indexes.len(), 1);
    let i = &edge.indexes[0];
    assert_eq!(i.name.as_str(), "gx_edge_owner_rev");
    assert_eq!(i.fields.len(), 1);
    assert_eq!(i.fields[0].as_str(), "owner");
    assert!(!i.unique);
    assert_eq!(i.method, IndexMethod::BTree);
    assert!(index_names(&cat, "node").is_empty());
}

#[test]
fn an_empty_list_gives_the_catalogue_of_catalog() {
    let m = meta(true);
    let plain = m.catalog(en()).expect("plain");
    assert_eq!(
        plain,
        m.catalog_with_reverse_indexes(en(), &[]).expect("with")
    );
    let caps = spot_storage_contract::Capabilities {
        backend: "memory".into(),
        contract_version: "0.2.0".into(),
        read_sources: vec![],
        max_writes_per_tx: 100,
        feed_retention: std::time::Duration::from_secs(60),
        geo_filters: false,
        range_exclusion: true,
        search: true,
    };
    assert_eq!(
        m.catalog_for(en(), &caps).expect("for"),
        m.catalog_for_with_reverse_indexes(en(), &caps, &[])
            .expect("for with")
    );
}

#[test]
fn an_index_true_field_keeps_its_own_index_and_gets_the_entrys_too() {
    let cat = meta(true)
        .catalog_with_reverse_indexes(en(), &[ri("edge", "owner", "gx_edge_owner_rev")])
        .expect("catalog");
    assert_eq!(
        index_names(&cat, "edge"),
        ["ix_edge_owner", "gx_edge_owner_rev"]
    );
}

#[test]
fn unknown_doctype_non_link_field_and_name_clashes_are_compile_errors() {
    let m = meta(true);
    for bad in [
        ri("ghost", "owner", "gx_a"),
        ri("edge", "nope", "gx_a"),
        ri("edge", "note", "gx_a"),
        // The name of the field's own index.
        ri("edge", "owner", "ix_edge_owner"),
    ] {
        let e = m
            .catalog_with_reverse_indexes(en(), &[bad])
            .expect_err("must refuse");
        assert_eq!(e.kind, MetaErrorKind::Compile);
    }
    // One name on two columns.
    let mut d = defs(false);
    d[0].fields.push(of(FieldType::Link, "other", Some("node")));
    let m = build(d).expect("meta builds");
    let e = m
        .catalog_with_reverse_indexes(
            en(),
            &[ri("edge", "owner", "gx_a"), ri("edge", "other", "gx_a")],
        )
        .expect_err("two columns");
    assert_eq!(e.kind, MetaErrorKind::Compile);
}
