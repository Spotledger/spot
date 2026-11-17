//! Search served end to end through `PgTransaction` (WP-STO-13.T3, ADR-087 section 7): a backend
//! with the [`SearchHook`] installed prepares the projection, keeps it in step with every document
//! write and delete in the same transaction, and answers `Expr::Search` instead of `Unsupported`.
//!
//! The wiring tests with a recording fake (when the hook is called, a failing hook, no hook) are in
//! `spot-postgres/tests/search_wiring.rs`; the brief names that file for them, and a test of the
//! real projection cannot live there because this crate depends on `spot-postgres`.
//!
//! Skipped without `SPOT_TEST_PG_URL`; under `SPOT_REQUIRE_PG=1` a server without `unaccent` fails
//! the run (the harness in `common` decides).
//!
//! ```text
//! SPOT_TEST_PG_URL=postgres://user:pass@localhost:5432/postgres SPOT_REQUIRE_PG=1 \
//!   cargo test -p spot-pg-search --test search_wiring
//! ```
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::n;
use spot_pg_search::{Language, Protection, SearchHook};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, ErrorKind, Expr, FieldRef, Fields, Isolation, Query,
    QueryExecutor, QueryPath, ReadSource, RecordId, SearchMode, SearchTerm, Selection,
    StorageBackend, Transaction, TxOptions, Value,
};

/// A scratch database with the extensions, a backend with the hook installed and the catalog
/// prepared through it: no fixture builds a table or a projection by hand.
struct Wired {
    db: TestDb,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

async fn wired() -> Option<Wired> {
    let db = TestDb::create().await?;
    if !common::require_extensions(&db).await {
        return None;
    }
    let protection = Protection::new()
        .group_field(&n("listing"), &n("owner_note"), "owner")
        .unwrap();
    let backend = SearchHook::install(
        PgBackend::connect(db.config()).await.expect("connect"),
        // `Expr::Search` parses with `simple`, so the projection must be built with it (plan.rs).
        Language::new("ur").unwrap(),
        protection,
    );
    let catalog = common::catalog();
    backend.prepare(&catalog).await.expect("prepare");
    Some(Wired {
        db,
        backend,
        catalog,
    })
}

async fn begin(w: &Wired) -> spot_postgres::PgTransaction {
    let options = TxOptions::new(n("search_wiring"), Isolation::ReadCommitted);
    w.backend
        .begin(Arc::clone(&w.catalog), options)
        .await
        .expect("begin")
}

fn listing(id: RecordId, title: &str, body: Option<&str>, note: Option<&str>) -> DocumentData {
    let text = |v: Option<&str>| v.map_or(Value::Null, |x| Value::Text(x.to_owned()));
    let mut fields = Fields::new();
    fields.insert(n("title"), Value::Text(title.to_owned()));
    fields.insert(n("body"), text(body));
    fields.insert(n("owner_note"), text(note));
    fields.insert(n("price_band"), Value::Text("cheap".to_owned()));
    fields.insert(n("guests"), Value::Int(2));
    DocumentData {
        entity: n("listing"),
        id,
        fields,
        children: Default::default(),
    }
}

/// `Expr::Search` over the title and body, selecting the ids.
fn search_query(text: &str) -> Query {
    let mut query = Query::from(&n("listing"), &n("s"), QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::Field(FieldRef {
            alias: n("s"),
            field: n("id"),
        }),
        alias: n("id"),
    }];
    query.filter = Some(Expr::Search(SearchTerm {
        mode: SearchMode::Web,
        text: text.to_owned(),
        fields: vec![n("title"), n("body")],
    }));
    query
}

/// The ids a search finds inside `tx`, sorted.
async fn find_in(tx: &mut spot_postgres::PgTransaction, text: &str) -> Vec<RecordId> {
    let rows = tx.query(&search_query(text)).await.expect("search");
    let mut ids: Vec<RecordId> = rows
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Id(id) => *id,
            other => panic!("expected an id, got {other:?}"),
        })
        .collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<RecordId>) -> Vec<RecordId> {
    ids.sort();
    ids
}

async fn projection_rows(w: &Wired) -> i64 {
    let client = w.db.raw_client().await;
    client
        .query_one("SELECT count(*) FROM \"spot_search_doc\"", &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn prepare_creates_the_projection_and_the_backend_says_it_serves_search() {
    let Some(w) = wired().await else {
        return;
    };
    assert!(w.backend.capabilities().search);
    let client = w.db.raw_client().await;
    for object in ["spot_search_doc", "spot_search_doc_terms_gin"] {
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&object])
            .await
            .unwrap()
            .get(0);
        assert!(exists, "`{object}` must exist after prepare");
    }
    // Preparing again is idempotent.
    w.backend.prepare(&w.catalog).await.expect("prepare again");
}

#[tokio::test]
async fn an_inserted_document_is_found_in_the_same_transaction_and_after_commit() {
    let Some(w) = wired().await else {
        return;
    };
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    let mut tx = begin(&w).await;
    tx.insert(&listing(a, "Lighthouse keeper cottage", None, None))
        .await
        .unwrap();
    tx.insert(&listing(
        b,
        "Orchard barn",
        Some("near the lighthouse"),
        None,
    ))
    .await
    .unwrap();
    // Same transaction, uncommitted: both are visible to the next search.
    assert_eq!(find_in(&mut tx, "lighthouse").await, sorted(vec![a, b]));
    assert_eq!(find_in(&mut tx, "orchard").await, vec![b]);
    tx.commit().await.unwrap();

    let mut after = begin(&w).await;
    assert_eq!(find_in(&mut after, "lighthouse").await, sorted(vec![a, b]));
    after.rollback().await.unwrap();

    // `read` outside a transaction serves search too.
    let rows = w
        .backend
        .read(
            Arc::clone(&w.catalog),
            ReadSource::Primary,
            &search_query("orchard"),
        )
        .await
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Id(b)]]);
}

#[tokio::test]
async fn an_update_changes_the_match_and_a_delete_removes_the_hit() {
    let Some(w) = wired().await else {
        return;
    };
    let id = RecordId::new_v7();
    let mut tx = begin(&w).await;
    let version = tx
        .insert(&listing(id, "Lighthouse keeper cottage", None, None))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = begin(&w).await;
    let renamed = listing(id, "Orchard keeper cottage", None, None);
    let version = tx.update(&renamed, version).await.unwrap();
    assert!(find_in(&mut tx, "lighthouse").await.is_empty());
    assert_eq!(find_in(&mut tx, "orchard").await, vec![id]);
    tx.commit().await.unwrap();
    assert_eq!(projection_rows(&w).await, 1, "an update rewrites the row");

    let mut tx = begin(&w).await;
    tx.delete(&n("listing"), id, version).await.unwrap();
    assert!(find_in(&mut tx, "orchard").await.is_empty());
    tx.commit().await.unwrap();
    assert_eq!(projection_rows(&w).await, 0, "a delete removes the row");
}

#[tokio::test]
async fn a_rollback_leaves_no_projection_row() {
    let Some(w) = wired().await else {
        return;
    };
    let id = RecordId::new_v7();
    let mut tx = begin(&w).await;
    tx.insert(&listing(id, "Lighthouse keeper cottage", None, None))
        .await
        .unwrap();
    assert_eq!(find_in(&mut tx, "lighthouse").await, vec![id]);
    tx.rollback().await.unwrap();
    assert_eq!(projection_rows(&w).await, 0);

    // A rolled-back delete keeps the hit.
    let mut tx = begin(&w).await;
    let version = tx
        .insert(&listing(id, "Lighthouse keeper cottage", None, None))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(&w).await;
    tx.delete(&n("listing"), id, version).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = begin(&w).await;
    assert_eq!(find_in(&mut tx, "lighthouse").await, vec![id]);
    tx.rollback().await.unwrap();
    assert_eq!(projection_rows(&w).await, 1);
}

#[tokio::test]
async fn a_protected_field_never_enters_the_projection() {
    let Some(w) = wired().await else {
        return;
    };
    let id = RecordId::new_v7();
    let mut tx = begin(&w).await;
    // The token is only in the protected column; the writer may have been allowed to write it,
    // but no search may ever match it.
    tx.insert(&listing(
        id,
        "Quiet cottage",
        None,
        Some("zephyrtoken secret"),
    ))
    .await
    .unwrap();
    assert!(find_in(&mut tx, "zephyrtoken").await.is_empty());
    assert_eq!(find_in(&mut tx, "quiet").await, vec![id]);
    tx.commit().await.unwrap();

    let client = w.db.raw_client().await;
    let leaked: i64 = client
        .query_one(
            "SELECT count(*) FROM \"spot_search_doc\" WHERE \"terms\"::text LIKE '%zephyrtoken%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        leaked, 0,
        "the projection holds no text of a protected field"
    );
}

#[tokio::test]
async fn a_searchable_field_in_a_protection_group_refuses_prepare() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    if !common::require_extensions(&db).await {
        return;
    }
    let protection = Protection::new()
        .group_field(&n("listing"), &n("title"), "owner")
        .unwrap();
    let backend = SearchHook::install(
        PgBackend::connect(db.config()).await.expect("connect"),
        // `Expr::Search` parses with `simple`, so the projection must be built with it (plan.rs).
        Language::new("ur").unwrap(),
        protection,
    );
    let error = backend.prepare(&common::catalog()).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
}

#[tokio::test]
async fn a_server_without_the_extension_refuses_prepare_and_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // No `unaccent` here on purpose: the projection folds accents, so it is required.
    let backend = SearchHook::install(
        PgBackend::connect(db.config()).await.expect("connect"),
        // `Expr::Search` parses with `simple`, so the projection must be built with it (plan.rs).
        Language::new("ur").unwrap(),
        Protection::new(),
    );
    let error = backend.prepare(&common::catalog()).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
    assert!(error.explanation.contains("unaccent"), "{error}");
    let client = db.raw_client().await;
    let tables: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_tables WHERE tablename IN ('listing', 'spot_search_doc')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(tables, 0, "a refused prepare leaves no table behind");
}

#[tokio::test]
async fn a_backend_without_the_hook_still_refuses_search() {
    let Some(w) = wired().await else {
        return;
    };
    // Another backend on the same database, with no hook: search is `Unsupported` there.
    let plain = PgBackend::connect(w.db.config()).await.expect("connect");
    assert!(!plain.capabilities().search);
    let options = TxOptions::new(n("no_hook"), Isolation::ReadCommitted);
    let mut tx = plain
        .begin(Arc::clone(&w.catalog), options)
        .await
        .expect("begin");
    let error = tx.query(&search_query("lighthouse")).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    tx.rollback().await.unwrap();
}
