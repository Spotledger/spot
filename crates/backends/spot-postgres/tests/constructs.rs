//! Geo points, ranges and exclusions (ADR-087, WP-STO-03.T7): the columns `prepare` builds, the
//! values they round-trip, the exclusion constraint and its error, and the extension checks.
//!
//! Range and exclusion tests need `btree_gist`; geo tests need `postgis`. A server that does not
//! offer an extension skips the tests that need it, printing `SKIPPED:` (the CI image that
//! installs `postgis` is WP-STO-10.T4's). The refusal tests need no extension at all: they run
//! on a database where the extension is deliberately not created.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;

use documents_support::*;
use rust_decimal::Decimal;
use spot_storage_contract::contract_tests::{geo_catalog, range_catalog};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, EntityDef, EntityKind, ErrorKind, FieldDef,
    FieldType, Fields, GeoPoint, IndexDef, IndexMethod, RangeOf, RangePoint, RangeValue, RecordId,
    StorageBackend, StorageError, Transaction, Value,
};
use time::{Date, Month, OffsetDateTime, Time};
use tokio_postgres::Client;

/// A scratch database with `extensions` created, or `None` (printing why) when there is no
/// server or the server does not offer one of them.
async fn env_with_extensions(extensions: &[&str]) -> Option<Env> {
    let env = common::env().await?;
    let missing = env.db.create_extensions(extensions).await;
    if missing.is_empty() {
        Some(env)
    } else {
        println!("SKIPPED: the server does not offer {missing:?}");
        None
    }
}

fn catalog_of(version: u64, entities: Vec<EntityDef>) -> Arc<Catalog> {
    Arc::new(Catalog::new(version, Collation::new("en").unwrap(), entities).unwrap())
}

fn day(d: u8) -> Date {
    Date::from_calendar_date(2026, Month::November, d).unwrap()
}

fn nights(from: u8, to: u8) -> Value {
    Value::Range(RangeValue::half_open(
        RangePoint::Date(day(from)),
        RangePoint::Date(day(to)),
    ))
}

fn stay(unit: Option<&str>, during: Value) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("unit"), unit.map_or(Value::Null, Value::from));
    fields.insert(n("during"), during);
    DocumentData {
        entity: n("stay"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

fn place(name: &str, pin: Value) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), name.into());
    fields.insert(n("pin"), pin);
    DocumentData {
        entity: n("place"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

fn point(lon: (i64, u32), lat: (i64, u32)) -> Value {
    Value::GeoPoint(GeoPoint::new(
        Decimal::new(lon.0, lon.1),
        Decimal::new(lat.0, lat.1),
    ))
}

async fn text(client: &Client, sql: &str) -> String {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

async fn objects(client: &Client) -> BTreeSet<(String, u32)> {
    let rows = client
        .query(
            "SELECT 'class', oid FROM pg_class WHERE relnamespace = 'public'::regnamespace \
             UNION ALL SELECT 'constraint', oid FROM pg_constraint \
                 WHERE connamespace = 'public'::regnamespace \
             UNION ALL SELECT 'trigger', oid FROM pg_trigger WHERE NOT tgisinternal",
            &[],
        )
        .await
        .unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

async fn insert_commit(
    env: &Env,
    cat: &Arc<Catalog>,
    doc: &DocumentData,
) -> Result<(), StorageError> {
    let mut tx = env
        .backend
        .begin(
            cat.clone(),
            spot_storage_contract::TxOptions::new(
                n("constructs_test"),
                spot_storage_contract::Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap();
    match tx.insert(doc).await {
        Ok(_) => tx.commit().await,
        Err(e) => {
            tx.rollback().await.unwrap();
            Err(e)
        }
    }
}

// --- the extension checks --------------------------------------------------------------------

#[tokio::test]
async fn prepare_without_postgis_fails_naming_the_extension_and_the_install_operation() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = Arc::new(geo_catalog().unwrap());
    let error = env.backend.prepare(&cat).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
    assert!(error.explanation.contains("postgis"), "{error}");
    assert!(error.explanation.contains("extension.install"), "{error}");
    assert_eq!(error.entity, Some(n("place")));
    // Nothing was built, and in particular no text column stands in for the point.
    let client = env.db.raw_client().await;
    let exists: bool = client
        .query_one("SELECT to_regclass('place') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!exists, "a refused prepare must leave no table behind");
}

#[tokio::test]
async fn a_scoped_exclusion_without_btree_gist_is_refused_before_any_change() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    let error = env.backend.prepare(&cat).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
    assert!(error.explanation.contains("btree_gist"), "{error}");
    assert!(error.explanation.contains("extension.install"), "{error}");
    let client = env.db.raw_client().await;
    let exists: bool = client
        .query_one("SELECT to_regclass('stay') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!exists);
}

#[tokio::test]
async fn a_range_field_and_a_single_column_exclusion_need_no_extension() {
    let Some(env) = common::env().await else {
        return;
    };
    let mut entity = range_catalog().unwrap().entities().next().unwrap().clone();
    entity.exclusions[0].scope.clear();
    let cat = catalog_of(1, vec![entity]);
    env.backend.prepare(&cat).await.unwrap();
    insert_commit(&env, &cat, &stay(Some("a"), nights(1, 3)))
        .await
        .unwrap();
    let error = insert_commit(&env, &cat, &stay(Some("b"), nights(2, 4)))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ExclusionViolation, "{error}");
}

#[tokio::test]
async fn a_search_declaration_is_unsupported_not_silently_ignored() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = Arc::new(spot_storage_contract::contract_tests::search_catalog().unwrap());
    let error = env.backend.prepare(&cat).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    assert!(!env.backend.capabilities().search);
}

#[tokio::test]
async fn capabilities_advertise_geo_and_range_and_not_search() {
    let Some(env) = common::env().await else {
        return;
    };
    let caps = env.backend.capabilities();
    assert!(caps.geo_filters && caps.range_exclusion);
    assert!(!caps.search);
}

// --- ranges and exclusions -------------------------------------------------------------------

#[tokio::test]
async fn the_exclusion_is_a_gist_constraint_over_the_scope_and_the_range() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();
    let c = env.db.raw_client().await;
    assert_eq!(
        text(
            &c,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'spot_stay_stay_unit_during_excl_excl' AND contype = 'x'"
        )
        .await,
        "EXCLUDE USING gist (unit WITH =, during WITH &&)"
    );
    assert_eq!(
        text(
            &c,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'stay'::regclass AND attname = 'during'"
        )
        .await,
        "daterange"
    );
}

#[tokio::test]
async fn adjacent_ranges_commit_overlapping_ones_do_not_and_null_never_conflicts() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();

    let first = stay(Some("u1"), nights(1, 3));
    let touching = stay(Some("u1"), nights(3, 5));
    insert_commit(&env, &cat, &first).await.unwrap();
    insert_commit(&env, &cat, &touching).await.unwrap();

    let overlap = stay(Some("u1"), nights(2, 4));
    let error = insert_commit(&env, &cat, &overlap).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::ExclusionViolation, "{error}");
    assert_eq!(error.entity, Some(n("stay")));
    assert_eq!(error.field, Some(n("during")));
    assert_eq!(error.constraint, Some(n("stay_unit_during_excl")));
    let shown = error.to_string();
    assert!(shown.contains("already taken"), "{shown}");
    for other in [first.id, touching.id] {
        assert!(!shown.contains(&other.to_string()), "{shown}");
    }
    assert!(
        !shown.contains("2026-11"),
        "no conflicting value in: {shown}"
    );

    // Another scope is free, and a null range or a null scope never conflicts.
    insert_commit(&env, &cat, &stay(Some("u2"), nights(2, 4)))
        .await
        .unwrap();
    insert_commit(&env, &cat, &stay(Some("u1"), Value::Null))
        .await
        .unwrap();
    insert_commit(&env, &cat, &stay(Some("u1"), Value::Null))
        .await
        .unwrap();
    insert_commit(&env, &cat, &stay(None, nights(1, 3)))
        .await
        .unwrap();
    insert_commit(&env, &cat, &stay(None, nights(1, 3)))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_update_into_a_conflict_is_an_exclusion_violation_too() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();
    let held = stay(Some("u1"), nights(1, 3));
    let mover = stay(Some("u1"), nights(5, 7));
    seed(&env, &cat, &[&held, &mover]).await;
    let mut tx = begin(&env, &cat).await;
    let mut moved = mover.clone();
    moved.fields.insert(n("during"), nights(2, 6));
    let error = tx
        .update(&moved, spot_storage_contract::Version::FIRST)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ExclusionViolation, "{error}");
    assert_eq!(error.field, Some(n("during")));
}

#[tokio::test]
async fn ranges_that_are_not_half_open_finite_and_non_empty_are_refused() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();
    let d = |x: u8| RangePoint::Date(day(x));
    let bad = [
        ("empty", RangeValue::half_open(d(3), d(3))),
        ("reversed", RangeValue::half_open(d(5), d(3))),
        (
            "equal but inclusive [3,3]",
            RangeValue {
                lower: Bound::Included(d(3)),
                upper: Bound::Included(d(3)),
            },
        ),
        (
            "unbounded above",
            RangeValue {
                lower: Bound::Included(d(3)),
                upper: Bound::Unbounded,
            },
        ),
        (
            "unbounded below",
            RangeValue {
                lower: Bound::Unbounded,
                upper: Bound::Excluded(d(3)),
            },
        ),
        (
            "closed on both ends",
            RangeValue {
                lower: Bound::Included(d(1)),
                upper: Bound::Included(d(3)),
            },
        ),
        (
            "open below",
            RangeValue {
                lower: Bound::Excluded(d(1)),
                upper: Bound::Excluded(d(3)),
            },
        ),
        (
            "of another kind",
            RangeValue::half_open(RangePoint::Int(1), RangePoint::Int(3)),
        ),
    ];
    for (what, range) in bad {
        let error = insert_commit(&env, &cat, &stay(Some("u"), Value::Range(range)))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidValue, "{what}: {error}");
        assert_eq!(error.field, Some(n("during")), "{what}");
    }
}

/// An entity with one range field of each kind.
fn all_ranges() -> Arc<Catalog> {
    let field = |name: &str, of: RangeOf| FieldDef {
        name: n(name),
        ty: FieldType::Range { of },
        nullable: true,
    };
    catalog_of(
        1,
        vec![EntityDef {
            name: n("window"),
            kind: EntityKind::Document,
            fields: vec![
                field("ints", RangeOf::Int),
                field("decimals", RangeOf::Decimal),
                field("dates", RangeOf::Date),
                field("instants", RangeOf::DateTime),
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        }],
    )
}

#[tokio::test]
async fn ranges_of_every_kind_come_back_half_open_and_exact() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = all_ranges();
    env.backend.prepare(&cat).await.unwrap();
    let instant = |h: u8, micros: u32| {
        OffsetDateTime::new_utc(day(1), Time::from_hms_micro(h, 30, 15, micros).unwrap())
    };
    let mut fields = Fields::new();
    fields.insert(
        n("ints"),
        Value::Range(RangeValue::half_open(
            RangePoint::Int(i64::MIN + 1),
            RangePoint::Int(i64::MAX),
        )),
    );
    fields.insert(
        n("decimals"),
        Value::Range(RangeValue::half_open(
            RangePoint::Decimal(Decimal::new(-1_234_567_891, 7)),
            RangePoint::Decimal(Decimal::new(100, 2)),
        )),
    );
    fields.insert(n("dates"), nights(1, 30));
    fields.insert(
        n("instants"),
        Value::Range(RangeValue::half_open(
            RangePoint::DateTime(instant(8, 123_456)),
            RangePoint::DateTime(instant(9, 1)),
        )),
    );
    let doc = DocumentData {
        entity: n("window"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    };
    seed(&env, &cat, &[&doc]).await;
    let (stored, _) = load(&env, &cat, "window", doc.id).await.unwrap();
    assert_eq!(stored, doc);

    // A date range [1,3) is stored as exactly that, not canonicalised to a closed upper bound.
    let c = env.db.raw_client().await;
    assert_eq!(
        text(&c, "SELECT dates::text FROM \"window\"").await,
        "[2026-11-01,2026-11-30)"
    );

    // Null is a value of its own.
    let mut nulls = doc.clone();
    nulls.id = RecordId::new_v7();
    for f in ["ints", "decimals", "dates", "instants"] {
        nulls.fields.insert(n(f), Value::Null);
    }
    seed(&env, &cat, &[&nulls]).await;
    let (stored, _) = load(&env, &cat, "window", nulls.id).await.unwrap();
    assert_eq!(stored, nulls);
}

#[tokio::test]
async fn preparing_a_range_catalog_again_changes_nothing() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();
    let c = env.db.raw_client().await;
    let before = objects(&c).await;
    env.backend.prepare(&cat).await.unwrap();
    env.backend.prepare(&cat).await.unwrap();
    assert_eq!(objects(&c).await, before);
}

#[tokio::test]
async fn a_changed_or_removed_exclusion_is_refused_not_altered() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let cat = Arc::new(range_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();

    let mut changed = cat.entities().next().unwrap().clone();
    changed.exclusions[0].scope.clear();
    let error = env
        .backend
        .prepare(&catalog_of(2, vec![changed]))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
    assert!(
        error.explanation.contains("different definition"),
        "{error}"
    );

    let mut removed = cat.entities().next().unwrap().clone();
    removed.exclusions.clear();
    let error = env
        .backend
        .prepare(&catalog_of(2, vec![removed]))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
    assert!(
        error.explanation.contains("dropping an exclusion"),
        "{error}"
    );
}

#[tokio::test]
async fn an_exclusion_added_later_checks_the_rows_already_there() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let full = range_catalog().unwrap().entities().next().unwrap().clone();
    let mut bare = full.clone();
    bare.exclusions.clear();
    env.backend
        .prepare(&catalog_of(1, vec![bare.clone()]))
        .await
        .unwrap();
    // Without the exclusion two overlapping stays commit.
    let cat = catalog_of(1, vec![bare]);
    insert_commit(&env, &cat, &stay(Some("u1"), nights(1, 3)))
        .await
        .unwrap();
    insert_commit(&env, &cat, &stay(Some("u1"), nights(2, 4)))
        .await
        .unwrap();
    let error = env
        .backend
        .prepare(&catalog_of(2, vec![full]))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ExclusionViolation, "{error}");
}

// --- geo points ------------------------------------------------------------------------------

#[tokio::test]
async fn geo_points_round_trip_exactly_and_null_is_not_the_origin() {
    let Some(env) = env_with_extensions(&["postgis"]).await else {
        return;
    };
    let cat = Arc::new(geo_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();
    let docs = [
        place("exact", point((739_000_001, 7), (-338_688_197, 7))),
        place("origin", point((0, 0), (0, 0))),
        place("edge", point((180, 0), (-90, 0))),
        place("small", point((1, 7), (-1, 7))),
        place("unknown", Value::Null),
    ];
    seed(&env, &cat, &docs.iter().collect::<Vec<_>>()).await;
    for d in &docs {
        let (stored, _) = load(&env, &cat, "place", d.id).await.unwrap();
        assert_eq!(&stored, d);
    }
    let (null, _) = load(&env, &cat, "place", docs[4].id).await.unwrap();
    assert_eq!(null.fields.get(&n("pin")), Some(&Value::Null));
}

#[tokio::test]
async fn the_point_column_is_geometry_4326_and_a_second_prepare_changes_nothing() {
    let Some(env) = env_with_extensions(&["postgis"]).await else {
        return;
    };
    let cat = Arc::new(geo_catalog().unwrap());
    env.backend.prepare(&cat).await.unwrap();
    let c = env.db.raw_client().await;
    assert_eq!(
        text(
            &c,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'place'::regclass AND attname = 'pin'"
        )
        .await
        .replace("public.", ""),
        "geometry(Point,4326)"
    );
    // The declared GiST index has the physical name spot-pg-migrate plans.
    assert_eq!(
        text(
            &c,
            "SELECT am.amname::text FROM pg_class c JOIN pg_am am ON am.oid = c.relam \
             WHERE c.relname = 'spot_place_place_pin_gix'"
        )
        .await,
        "gist"
    );
    let before = objects(&c).await;
    env.backend.prepare(&cat).await.unwrap();
    assert_eq!(objects(&c).await, before);
}

#[tokio::test]
async fn a_gist_index_gets_its_migrator_name_and_a_second_prepare_changes_nothing() {
    let Some(env) = env_with_extensions(&["btree_gist"]).await else {
        return;
    };
    let mut entity = range_catalog().unwrap().entities().next().unwrap().clone();
    let mut gist = IndexDef::btree(n("stay_unit_ix"), vec![n("unit")], false);
    gist.method = IndexMethod::Gist;
    gist.opclass = Some(spot_storage_contract::OpClass::GistScalar);
    entity.indexes = vec![gist.clone()];
    let cat = catalog_of(1, vec![entity.clone()]);
    env.backend.prepare(&cat).await.unwrap();
    let c = env.db.raw_client().await;
    assert_eq!(
        text(
            &c,
            "SELECT am.amname::text FROM pg_class c JOIN pg_am am ON am.oid = c.relam \
             WHERE c.relname = 'spot_stay_stay_unit_ix_gix'"
        )
        .await,
        "gist"
    );
    let before = objects(&c).await;
    env.backend.prepare(&cat).await.unwrap();
    assert_eq!(objects(&c).await, before);
}
