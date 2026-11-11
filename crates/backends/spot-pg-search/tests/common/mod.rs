//! Shared fixture: a scratch database, a searchable DocType, its entity table and its projection.
//!
//! `spot-postgres`'s own `prepare` still refuses a catalog with a `search` declaration
//! (`schema::refuse_search`, WP-STO-13.T1's flag to remove), so the entity table here is created
//! by the fixture with the columns `prepare` builds: `id uuid` primary key, `version bigint`, the
//! declared text columns and their length checks. Everything the projection does is against the
//! same table a prepared catalog would give it.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_pg_search::{Language, ProjectionPlan, Protection, prepare};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, Isolation, Name, SearchDef,
    SearchField, SearchWeight, StorageBackend, Transaction, TxOptions,
};

pub fn n(text: &str) -> Name {
    Name::new(text).unwrap_or_else(|e| panic!("{e}"))
}

/// The catalog every test searches: `listing` with an English title, a body that the tests make
/// Urdu, a protected column (`owner_note`, never searchable), and two facets. `typo` is the
/// declaration's `typo_tolerance`, which the query compiler reads — the crate's plan copies it,
/// so both must agree.
pub fn catalog_with(typo: bool) -> Arc<Catalog> {
    let text = |name: &str, nullable: bool| FieldDef {
        name: n(name),
        ty: FieldType::Text { max_chars: None },
        nullable,
    };
    let entity = EntityDef {
        name: n("listing"),
        kind: EntityKind::Document,
        fields: vec![
            FieldDef {
                name: n("title"),
                ty: FieldType::Text {
                    max_chars: Some(140),
                },
                nullable: false,
            },
            text("body", true),
            text("owner_note", true),
            text("price_band", true),
            FieldDef {
                name: n("guests"),
                ty: FieldType::Int,
                nullable: true,
            },
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: Some(SearchDef {
            language: None,
            fields: vec![
                SearchField {
                    name: n("title"),
                    weight: SearchWeight::A,
                },
                SearchField {
                    name: n("body"),
                    weight: SearchWeight::D,
                },
            ],
            facets: vec![n("price_band"), n("guests")],
            typo_tolerance: typo,
        }),
    };
    Arc::new(Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap())
}

/// The catalog without typo tolerance.
pub fn catalog() -> Arc<Catalog> {
    catalog_with(false)
}

/// The plan: the title in English, the body in Urdu (PostgreSQL has no Urdu configuration, so it
/// is `simple`), and typo tolerance when `typo` asks for it.
pub fn plan(typo: bool) -> ProjectionPlan {
    let catalog = catalog_with(typo);
    let entity = catalog.entity(&n("listing")).unwrap();
    ProjectionPlan::from_entity(entity, &Language::new("en").unwrap(), &Protection::new())
        .unwrap()
        .unwrap()
        .with_field_language(&n("body"), Language::new("ur").unwrap())
        .unwrap()
}

/// A scratch database, the backend on it, the catalog and the prepared projection.
pub struct Env {
    pub db: TestDb,
    pub backend: PgBackend,
    pub catalog: Arc<Catalog>,
    pub plan: ProjectionPlan,
}

/// A prepared environment, or `None` (printing why) without a server or a needed extension.
pub async fn env(typo: bool) -> Option<Env> {
    let db = TestDb::create().await?;
    let missing = db.create_extensions(&["unaccent", "pg_trgm"]).await;
    if !missing.is_empty() {
        println!("SKIPPED: the server does not offer {missing:?}");
        return None;
    }
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let catalog = catalog_with(typo);
    let plan = plan(typo);
    entity_table(&db).await;
    let mut tx = begin(&backend, &catalog).await;
    prepare(&mut tx, std::slice::from_ref(&plan))
        .await
        .expect("prepare");
    tx.commit().await.expect("commit");
    Some(Env {
        db,
        backend,
        catalog,
        plan,
    })
}

/// The entity table `spot-postgres`'s `prepare` would create for [`catalog`], plus the tenant
/// collation its query compiler names.
pub async fn entity_table(db: &TestDb) {
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE COLLATION IF NOT EXISTS \"spot_en\" (provider = icu, locale = 'en', \
               deterministic = true);
             CREATE TABLE \"listing\" (
               \"id\" uuid NOT NULL,
               \"version\" bigint NOT NULL,
               \"title\" text NOT NULL,
               \"body\" text,
               \"owner_note\" text,
               \"price_band\" text,
               \"guests\" bigint,
               CONSTRAINT \"listing_pkey\" PRIMARY KEY (\"id\"),
               CONSTRAINT \"listing_title_len\" CHECK (char_length(\"title\") <= 140)
             );",
        )
        .await
        .expect("the entity table");
}

pub async fn begin(backend: &PgBackend, catalog: &Arc<Catalog>) -> PgTransaction {
    backend
        .begin(
            Arc::clone(catalog),
            TxOptions::new(n("search_test"), Isolation::ReadCommitted),
        )
        .await
        .expect("begin")
}

/// A stable UUIDv7 for row `i`, so a test can name a document without keeping a map.
pub fn uuid_for(i: i64) -> String {
    format!("00000000-0000-7000-8000-{i:012}")
}

/// Ten thousand rows: an English title on every one, an Urdu body on every third, a price band
/// that alternates and a guest count. Also every thousandth row carries `harbour` in its protected
/// column only, which no search may find.
pub async fn seed_ten_thousand(db: &TestDb) {
    let client = db.raw_client().await;
    client
        .batch_execute(
            "INSERT INTO \"listing\" (\"id\", \"version\", \"title\", \"body\", \"owner_note\", \
               \"price_band\", \"guests\") \
             SELECT ('00000000-0000-7000-8000-' || lpad(i::text, 12, '0'))::uuid, 1, \
               CASE WHEN i % 2 = 0 THEN 'Harbour cottage ' || i ELSE 'Seaside cottage ' || i END, \
               CASE WHEN i % 3 = 0 THEN 'ساحل سمندر ' || i ELSE 'plain body ' || i END, \
               CASE WHEN i % 1000 = 0 THEN 'harbour' ELSE NULL END, \
               CASE WHEN i % 2 = 0 THEN 'cheap' ELSE 'pricey' END, \
               i % 5 \
             FROM generate_series(1, 10000) AS i;",
        )
        .await
        .expect("the seeded rows");
}

/// Projects every row of the DocType through the crate's rebuild.
pub async fn project_all(env: &Env) {
    let mut tx = begin(&env.backend, &env.catalog).await;
    spot_pg_search::rebuild(&mut tx, &env.plan)
        .await
        .expect("rebuild");
    tx.commit().await.expect("commit");
}

/// An untyped query for assertions: one row, column `index`.
pub async fn scalar_i64(db: &TestDb, sql: &str) -> i64 {
    let client = db.raw_client().await;
    client
        .query_one(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}
