//! Indexes as usage objects: the manifest lists the catalogue's indexes, a table entry never
//! covers one, and the drop of an index is decided on the index alone.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{catalog_with_indexes, change, manifest, plan, release_id};
use spot_expand_contract::usage::check_manifest;
use spot_expand_contract::{
    ManifestProblemKind, Object, Phase, Release, UsageManifest, check, classify, codes,
};
use spot_migration_plan::risk::rules;
use spot_migration_plan::{ChangeKind, Risk};

const INDEX: &str = "gx_edge_parent_rev";

fn registry() -> spot_storage_contract::Catalog {
    catalog_with_indexes(&[("edge", &["src", "dst"])], &[("edge", INDEX, "src")])
}

fn drop_plan() -> spot_migration_plan::ReleasePlan {
    plan(vec![change(
        "schema/traversal_index/drop:edge.gx_edge_parent_rev",
        ChangeKind::SchemaDropIndex,
        rules::SCHEMA_DROP_TRAVERSAL_INDEX,
        "edge.gx_edge_parent_rev",
        Risk::Destructive,
    )])
}

#[test]
fn the_manifest_of_a_catalogue_lists_its_indexes_with_the_table_and_columns() {
    let derived = UsageManifest::from_catalog(release_id("1"), &registry());
    let text = derived.to_toml();
    assert!(text.contains("\"index:edge.gx_edge_parent_rev\""), "{text}");
    assert!(text.contains("\"edge\""));
    assert!(text.contains("\"edge.src\""));
    assert!(check_manifest(&derived, &registry()).is_empty());
}

#[test]
fn a_table_or_column_entry_never_covers_an_index() {
    let index = Object::index("edge", INDEX).unwrap();
    assert!(index.is_index());
    assert_eq!(index.to_string(), "edge.gx_edge_parent_rev");
    assert_eq!(index.manifest_text(), "index:edge.gx_edge_parent_rev");
    let tables = manifest("1", &["edge", "edge.gx_edge_parent_rev"]);
    assert!(!tables.uses(&index));
    let named = manifest("1", &["index:EDGE.Gx_Edge_Parent_Rev"]);
    assert!(named.uses(&index));
    // And an index entry covers neither the table nor a column of the same name.
    assert!(!named.uses(&Object::table("edge").unwrap()));
    assert!(!named.uses(&Object::column("edge", INDEX).unwrap()));
}

#[test]
fn the_drop_of_an_index_is_decided_on_the_index_alone() {
    let release_plan = drop_plan();
    assert_eq!(classify(&release_plan)[0].phase, Phase::Contract);
    assert_eq!(
        classify(&release_plan)[0].object,
        Some(Object::index("edge", INDEX).unwrap())
    );
    let id = release_id("2");
    let table = "edge";
    let index = "index:edge.gx_edge_parent_rev";

    // The current release still lists the index: refused while used.
    let usage = manifest("2", &[table, index]);
    let previous = manifest("1", &[table, index]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(&release, &previous);
    assert_eq!(report.refusals[0].code, codes::CONTRACT_WHILE_USED);

    // Only the previous release lists it: too early.
    let usage = manifest("2", &[table]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(&release, &previous);
    assert_eq!(report.refusals[0].code, codes::CONTRACT_TOO_EARLY);

    // Neither lists it, the table is still in both: accepted.
    let previous = manifest("1", &[table]);
    let report = check(&release, &previous);
    assert!(report.is_ok(), "{}", report.render_text());
    assert_eq!(report.accepted.len(), 1);
}

#[test]
fn a_column_drop_and_a_table_drop_are_still_decided_by_their_own_entries() {
    let id = release_id("2");
    let release_plan = plan(vec![
        change(
            "c",
            ChangeKind::SchemaBlocked,
            "schema/blocked",
            "edge.dst",
            Risk::Destructive,
        ),
        change(
            "t",
            ChangeKind::DocTypeRemoved,
            "metadata/doctype_removed",
            "other",
            Risk::Destructive,
        ),
    ]);
    let usage = manifest("2", &["edge", "index:edge.gx_edge_parent_rev"]);
    let previous = manifest("1", &["other"]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(&release, &previous);
    // The table entry covers the column: still used. The table `other` is too early.
    assert_eq!(report.refusals[0].code, codes::CONTRACT_WHILE_USED);
    assert_eq!(report.refusals[1].code, codes::CONTRACT_TOO_EARLY);
}

#[test]
fn index_entries_round_trip_in_a_stable_order() {
    let first = UsageManifest::from_toml(
        "release = \"1\"\nobjects = [\"index:edge.b_idx\", \"edge.src\", \"edge\", \"index:edge.a_idx\"]\n",
    )
    .unwrap();
    let text = first.to_toml();
    assert_eq!(text, first.to_toml());
    assert!(text.contains("index:edge.a_idx"));
    let second = UsageManifest::from_toml(&text).unwrap();
    assert_eq!(first, second);
    assert_eq!(second.len(), 4);
    assert!(Object::parse("index:edge").is_err());
    assert!(Object::parse("index:edge.a.b").is_err());
}

#[test]
fn check_manifest_flags_a_missing_and_a_stale_index() {
    let registry = registry();
    // A manifest that predates indexes: tables and columns only.
    let old = manifest("1", &["edge"]);
    let problems = check_manifest(&old, &registry);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].kind, ManifestProblemKind::MissingInManifest);
    assert_eq!(problems[0].object, Object::index("edge", INDEX).unwrap());
    assert!(problems[0].detail.contains("from_catalog"));

    let stale = manifest(
        "1",
        &[
            "edge",
            "index:edge.gx_edge_parent_rev",
            "index:edge.gone_idx",
        ],
    );
    let problems = check_manifest(&stale, &registry);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].kind, ManifestProblemKind::UnknownToCatalogue);
    assert_eq!(problems[0].object.to_string(), "edge.gone_idx");
}
