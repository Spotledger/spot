//! The index-coverage check on real plans (ADR-045, ADR-087 sections 4.4, 5.4, 7.3, 7.8 and 13
//! task 9): a transactional-path query must be served by an index, including a GIN index for a
//! search, a GiST index for a radius test and the exclusion constraint's GiST index for a range
//! overlap; a report-path query may scan, and the scan is marked.
//!
//! The queries are the compiler's own (`spot_pg_query::compile`), planned with their real
//! parameters bound, each as the typed value the backend sends. The tables hold 20,000 rows and
//! are analysed, so the planner chooses as it would on a live table; `enable_seqscan = off` is
//! deliberately not used, because the `ORDER BY id` every compiled query ends with lets a full
//! primary-key scan stand in for the scan and would hide the failures this gate exists to find.
//! The PostGIS and `btree_gist` cases skip with a message on a server that does not offer the
//! extension.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use rust_decimal::Decimal;
use spot_pg_query::coverage::{Coverage, PlanCoverage, analyse_plan, check, explain_statement};
use spot_pg_query::{CompiledQuery, DefaultSchemaMap, Param, PgType, compile};
use spot_storage_contract::contract_tests::{geo_catalog, range_catalog, search_catalog};
use spot_storage_contract::{
    Catalog, Collation, EntityDef, Expr, GeoTest, Name, Query, QueryPath, RangeTest, SearchMode,
    SearchTerm, StorageBackend, Value,
};
use time::{Date, Month};
use tokio_postgres::Client;
use tokio_postgres::types::{FromSql, ToSql, Type};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

/// The text of a `json` column: the wire form of `json` is its text.
struct JsonText(String);

impl<'a> FromSql<'a> for JsonText {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(JsonText(std::str::from_utf8(raw)?.to_owned()))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::JSON
    }
}

/// A compiled parameter as a driver value (only the types these queries use).
fn bind(params: &[Param]) -> Vec<Box<dyn ToSql + Sync + Send>> {
    params
        .iter()
        .map(|p| -> Box<dyn ToSql + Sync + Send> {
            match p {
                Param::Bool(v) => Box::new(*v),
                Param::Int8(v) => Box::new(*v),
                Param::Numeric(v) => Box::new(*v),
                Param::Text(v) => Box::new(v.clone()),
                Param::Date(v) => Box::new(*v),
                Param::TimestampTz(v) => Box::new(*v),
                other => panic!("the coverage fixtures do not bind {other:?}"),
            }
        })
        .collect()
}

/// The plan of `compiled`, read from `EXPLAIN (FORMAT JSON)` with its parameters bound.
async fn plan(client: &Client, compiled: &CompiledQuery) -> PlanCoverage {
    let values = bind(&compiled.params);
    // Typed as the compiler declares them (`$1::float8` over a numeric parameter, for example):
    // the server must not infer the types from the SQL, as it would not in the backend.
    let typed: Vec<(&(dyn ToSql + Sync), Type)> = values
        .iter()
        .zip(&compiled.params)
        .map(|(v, p)| (&**v as &(dyn ToSql + Sync), driver_type(p.pg_type())))
        .collect();
    let rows = client
        .query_typed(&explain_statement(&compiled.sql), &typed)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", compiled.sql));
    let json: JsonText = rows.first().expect("an EXPLAIN row").get(0);
    analyse_plan(&json.0).unwrap_or_else(|e| panic!("{e}\n{}", json.0))
}

/// This connection only: no parallel nodes and no JIT, which are machine-dependent noise in a plan.
async fn session(client: &Client) {
    client
        .batch_execute("SET max_parallel_workers_per_gather = 0; SET jit = off")
        .await
        .unwrap();
}

fn driver_type(ty: PgType) -> Type {
    match ty {
        PgType::Bool => Type::BOOL,
        PgType::Int8 => Type::INT8,
        PgType::Numeric => Type::NUMERIC,
        PgType::Text => Type::TEXT,
        PgType::Date => Type::DATE,
        PgType::TimestampTz => Type::TIMESTAMPTZ,
        other => panic!("the coverage fixtures do not bind {other:?}"),
    }
}

fn compiled(q: &Query, catalog: &Catalog) -> CompiledQuery {
    compile(q, catalog, &DefaultSchemaMap).unwrap_or_else(|e| panic!("{e}"))
}

fn catalog_of(entities: Vec<EntityDef>) -> Arc<Catalog> {
    Arc::new(Catalog::new(1, Collation::new("en").unwrap(), entities).unwrap())
}

fn day(d: u8) -> Value {
    Value::Date(Date::from_calendar_date(2026, Month::November, d).unwrap())
}

// --- radius and overlap ----------------------------------------------------------------------

/// A server with the extensions, `place` and `stay` prepared and loaded, or `None`.
async fn spatial_env() -> Option<(common::Env, Arc<Catalog>)> {
    let env = common::env().await?;
    let missing = env.db.create_extensions(&["postgis", "btree_gist"]).await;
    if !missing.is_empty() {
        println!("SKIPPED: the server does not offer {missing:?}");
        return None;
    }
    let mut entities: Vec<EntityDef> = geo_catalog().unwrap().entities().cloned().collect();
    entities.extend(range_catalog().unwrap().entities().cloned());
    let catalog = catalog_of(entities);
    env.backend.prepare(&catalog).await.unwrap();
    let client = env.db.raw_client().await;
    client
        .batch_execute(
            "INSERT INTO place (id, version, name, pin)
               SELECT gen_random_uuid(), 1, 'place-' || g,
                      ST_SetSRID(ST_MakePoint(60 + (g % 200) * 0.1, 20 + (g / 200) * 0.1), 4326)
               FROM generate_series(1, 20000) g;
             INSERT INTO stay (id, version, unit, during)
               SELECT gen_random_uuid(), 1, 'u' || (g % 50),
                      daterange(DATE '2026-01-01' + (g / 50) * 4,
                                DATE '2026-01-01' + (g / 50) * 4 + 3)
               FROM generate_series(1, 20000) g;
             ANALYZE place; ANALYZE stay;",
        )
        .await
        .unwrap();
    Some((env, catalog))
}

#[tokio::test]
async fn a_radius_query_uses_the_gist_index() {
    let Some((env, catalog)) = spatial_env().await else {
        return;
    };
    let client = env.db.raw_client().await;
    session(&client).await;
    let mut q = Query::from(&n("place"), &n("x"), QueryPath::Transactional);
    q.select = vec![spot_storage_contract::Selection {
        expr: Expr::field(&n("x"), &n("name")),
        alias: n("name"),
    }];
    q.filter = Some(Expr::Geo(
        Box::new(Expr::field(&n("x"), &n("pin"))),
        GeoTest::WithinDistance {
            lon: Decimal::new(670_000_000, 7),
            lat: Decimal::new(250_000_000, 7),
            metres: 2_000,
        },
    ));
    let c = compiled(&q, &catalog);
    let p = plan(&client, &c).await;
    println!("radius:\n{}\n{p:?}", c.sql);
    assert!(p.uses_index("spot_place_place_pin_gix"), "{p:?}");
    assert_eq!(check(QueryPath::Transactional, &p), Coverage::Covered);
}

#[tokio::test]
async fn an_overlap_query_uses_the_exclusion_constraints_index() {
    let Some((env, catalog)) = spatial_env().await else {
        return;
    };
    let client = env.db.raw_client().await;
    session(&client).await;
    let mut q = Query::from(&n("stay"), &n("x"), QueryPath::Transactional);
    q.select = vec![spot_storage_contract::Selection {
        expr: Expr::field(&n("x"), &n("unit")),
        alias: n("unit"),
    }];
    q.filter = Some(Expr::Range(
        Box::new(Expr::field(&n("x"), &n("during"))),
        RangeTest::Overlaps {
            start: day(1),
            end: day(5),
        },
    ));
    let c = compiled(&q, &catalog);
    let p = plan(&client, &c).await;
    println!("overlap:\n{}\n{p:?}", c.sql);
    assert!(
        p.uses_index("spot_stay_stay_unit_during_excl_excl"),
        "{p:?}"
    );
    assert_eq!(check(QueryPath::Transactional, &p), Coverage::Covered);
}

#[tokio::test]
async fn a_range_overlap_on_a_table_with_no_such_index_is_a_failure() {
    let Some((env, catalog)) = spatial_env().await else {
        return;
    };
    let client = env.db.raw_client().await;
    session(&client).await;
    // Drop the exclusion constraint (and with it its GiST index): no index serves the overlap.
    let dropped = client
        .query(
            "SELECT conname FROM pg_constraint WHERE conrelid = 'stay'::regclass AND contype = 'x'",
            &[],
        )
        .await
        .unwrap();
    for row in dropped {
        let name: String = row.get(0);
        client
            .batch_execute(&format!("ALTER TABLE stay DROP CONSTRAINT \"{name}\""))
            .await
            .unwrap();
    }
    let mut q = Query::from(&n("stay"), &n("x"), QueryPath::Transactional);
    q.select = vec![spot_storage_contract::Selection {
        expr: Expr::field(&n("x"), &n("unit")),
        alias: n("unit"),
    }];
    q.filter = Some(Expr::Range(
        Box::new(Expr::field(&n("x"), &n("during"))),
        RangeTest::Overlaps {
            start: day(1),
            end: day(5),
        },
    ));
    let p = plan(&client, &compiled(&q, &catalog)).await;
    assert!(!check(QueryPath::Transactional, &p).passes(), "{p:?}");
    let marked = check(QueryPath::Report, &p);
    assert!(matches!(marked, Coverage::ReportScan { .. }), "{marked}");
}

// --- search ----------------------------------------------------------------------------------

/// The projection of ADR-087 section 7.3 and the entity table, without the GIN index.
async fn search_env() -> Option<(common::Env, Arc<Catalog>)> {
    let env = common::env().await?;
    let client = env.db.raw_client().await;
    client
        .batch_execute(
            "CREATE TABLE listing (id uuid PRIMARY KEY, version bigint NOT NULL,
                                   title text NOT NULL, body text);
             CREATE TABLE spot_search_doc (doc_id uuid PRIMARY KEY, doctype text NOT NULL,
                                           terms tsvector NOT NULL, facets jsonb,
                                           tenant_language text, updated_at timestamptz);
             INSERT INTO listing (id, version, title, body)
               SELECT gen_random_uuid(), 1, 'title ' || g, 'body ' || g
               FROM generate_series(1, 20000) g;
             INSERT INTO spot_search_doc (doc_id, doctype, terms)
               SELECT id, 'listing',
                      to_tsvector('simple', title || CASE WHEN random() < 0.001
                                                          THEN ' harbour' ELSE '' END)
               FROM listing;
             ANALYZE listing; ANALYZE spot_search_doc;",
        )
        .await
        .unwrap();
    Some((env, Arc::new(search_catalog().unwrap())))
}

fn search_query(path: QueryPath) -> Query {
    let mut q = Query::from(&n("listing"), &n("x"), path);
    q.select = vec![spot_storage_contract::Selection {
        expr: Expr::field(&n("x"), &n("title")),
        alias: n("title"),
    }];
    q.filter = Some(Expr::Search(SearchTerm {
        mode: SearchMode::Web,
        text: "harbour".to_owned(),
        fields: vec![n("title")],
    }));
    q
}

#[tokio::test]
async fn a_transactional_search_with_no_gin_index_fails_the_gate() {
    let Some((env, catalog)) = search_env().await else {
        return;
    };
    let client = env.db.raw_client().await;
    session(&client).await;
    let c = compiled(&search_query(QueryPath::Transactional), &catalog);
    let p = plan(&client, &c).await;
    println!("search without GIN:\n{}\n{p:?}", c.sql);
    let verdict = check(QueryPath::Transactional, &p);
    assert!(!verdict.passes(), "{verdict}");
    let Coverage::Unindexed { relations } = verdict else {
        panic!("expected a failure");
    };
    assert!(
        relations.contains(&"spot_search_doc".to_owned()),
        "{relations:?}"
    );
}

#[tokio::test]
async fn a_transactional_search_with_the_gin_index_passes() {
    let Some((env, catalog)) = search_env().await else {
        return;
    };
    let client = env.db.raw_client().await;
    client
        .batch_execute(
            "CREATE INDEX spot_search_doc_terms_gin ON spot_search_doc USING GIN (terms);
             ANALYZE spot_search_doc;",
        )
        .await
        .unwrap();
    session(&client).await;
    let c = compiled(&search_query(QueryPath::Transactional), &catalog);
    let p = plan(&client, &c).await;
    println!("search with GIN:\n{}\n{p:?}", c.sql);
    assert!(p.uses_index("spot_search_doc_terms_gin"), "{p:?}");
    assert!(
        p.rechecks >= 1,
        "a GIN bitmap scan rechecks its rows: {p:?}"
    );
    assert_eq!(
        check(QueryPath::Transactional, &p),
        Coverage::Covered,
        "{p:?}"
    );
}

#[tokio::test]
async fn a_report_search_may_scan_and_the_scan_is_marked() {
    let Some((env, catalog)) = search_env().await else {
        return;
    };
    let client = env.db.raw_client().await;
    session(&client).await;
    let c = compiled(&search_query(QueryPath::Report), &catalog);
    let p = plan(&client, &c).await;
    let verdict = check(QueryPath::Report, &p);
    assert!(verdict.passes(), "{verdict}");
    assert!(matches!(verdict, Coverage::ReportScan { .. }), "{verdict}");
}
