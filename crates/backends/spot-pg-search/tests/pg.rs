//! The projection against a real PostgreSQL (ADR-087 section 7): Urdu and English over ten
//! thousand rows, the permission filter, stop words, typo tolerance only with `pg_trgm`, an
//! identical rebuild, maintenance inside the write's transaction, facets, the conformance checks
//! and the projection lag.
//!
//! Every test skips cleanly (printing one `SKIPPED:` line) when `SPOT_TEST_PG_URL` is unset, and
//! fails loudly when `SPOT_REQUIRE_PG=1` is set and the server is not there — the harness in
//! `spot-postgres::testing` decides. A server that does not offer `unaccent` or `pg_trgm` skips
//! the tests that need them, as the neighbouring crates do.
//!
//! ```text
//! SPOT_TEST_PG_URL=postgres://user:pass@localhost:5432/postgres SPOT_REQUIRE_PG=1 \
//!   cargo test -p spot-pg-search
//! ```
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::n;
use spot_pg_search::{
    ReadScope, RowScope, SearchRequest, check, measure_doctype, prepare, rebuild, remove, search,
    upsert,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    CmpOp, DocumentData, DocumentStore, ErrorKind, Expr, Fields, RecordId, SearchMode, SearchTerm,
    Transaction, Value, Version,
};

/// A consumer-box term: `Web` mode over the named fields.
fn term(text: &str, fields: &[&str]) -> SearchTerm {
    SearchTerm {
        mode: SearchMode::Web,
        text: text.to_owned(),
        fields: fields.iter().map(|f| n(f)).collect(),
    }
}

/// A scope over every row for the named fields.
fn scope(fields: &[&str]) -> ReadScope {
    ReadScope::all_rows(fields.iter().map(|f| n(f)))
}

/// A document of the fixture's `listing`, with every column set (nulls where `None`).
fn listing(id: RecordId, title: &str, body: Option<&str>, note: Option<&str>) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("title"), Value::Text(title.to_owned()));
    fields.insert(
        n("body"),
        body.map_or(Value::Null, |b| Value::Text(b.to_owned())),
    );
    fields.insert(
        n("owner_note"),
        note.map_or(Value::Null, |x| Value::Text(x.to_owned())),
    );
    fields.insert(n("price_band"), Value::Text("cheap".to_owned()));
    fields.insert(n("guests"), Value::Int(2));
    DocumentData {
        entity: n("listing"),
        id,
        fields,
        children: Default::default(),
    }
}

// --- ten thousand rows -------------------------------------------------------------------------

#[tokio::test]
async fn english_and_urdu_searches_answer_over_ten_thousand_rows() {
    let Some(env) = common::env(false).await else {
        return;
    };
    common::seed_ten_thousand(&env.db).await;
    common::project_all(&env).await;
    let projected = common::scalar_i64(&env.db, "SELECT count(*) FROM \"spot_search_doc\"").await;
    assert_eq!(projected, 10_000, "every row is projected");

    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let english = SearchRequest::new(term("harbour", &["title"])).with_limit(None);
    let page = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &scope(&["title"]),
        &english,
    )
    .await
    .expect("an English search over ten thousand rows");
    assert_eq!(page.hits.len(), 5_000, "half the titles say harbour");

    let urdu = SearchRequest::new(term("سمندر", &["body"])).with_limit(None);
    let page = search(&mut tx, &env.catalog, &env.plan, &scope(&["body"]), &urdu)
        .await
        .expect("an Urdu search over ten thousand rows");
    assert_eq!(page.hits.len(), 3_333, "every third body is Urdu");
    assert_eq!(page.lag.missing, 0);
    tx.rollback().await.expect("rollback");
}

// --- the permission filter ---------------------------------------------------------------------

#[tokio::test]
async fn a_protected_field_cannot_affect_a_match_or_a_rank() {
    let Some(env) = common::env(false).await else {
        return;
    };
    let (a, b, c) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    for doc in [
        listing(a, "harbour view", None, None),
        // The only `harbour` of this document is in a protected column.
        listing(b, "quiet street", None, Some("harbour owner")),
        listing(c, "harbour harbour view", None, None),
    ] {
        tx.insert(&doc).await.expect("insert");
    }
    rebuild(&mut tx, &env.plan).await.expect("rebuild");
    tx.commit().await.expect("commit");

    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term("harbour", &["title"])).with_limit(None);
    let page = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &scope(&["title"]),
        &request,
    )
    .await
    .expect("search");
    let ids: BTreeSet<RecordId> = page.hits.iter().map(|h| h.id).collect();
    assert_eq!(ids.len(), 2, "two documents match");
    assert!(!ids.contains(&b), "a protected field is not in the vector");
    assert!(ids.contains(&a) && ids.contains(&c));
    assert_eq!(
        page.hits.first().map(|h| h.id),
        Some(c),
        "the rank counts title occurrences only, and the protected column adds none"
    );
    tx.rollback().await.expect("rollback");

    // A search that names the protected field is refused, not narrowed.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term("harbour", &["owner_note"]));
    let error = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &scope(&["title"]),
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "permission.field_denied");
    tx.rollback().await.expect("rollback");
}

// --- SEM-SCH-01 --------------------------------------------------------------------------------

#[tokio::test]
async fn a_stop_word_term_matches_nothing_rather_than_everything() {
    let Some(env) = common::env(false).await else {
        return;
    };
    let id = RecordId::new_v7();
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    tx.insert(&listing(id, "the harbour", None, None))
        .await
        .expect("insert");
    rebuild(&mut tx, &env.plan).await.expect("rebuild");
    tx.commit().await.expect("commit");

    // The projection's English configuration drops `the` from the vector, so the term matches
    // nothing — it never degrades into "everything".
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let stop = SearchRequest::new(term("the", &["title"])).with_limit(None);
    let page = search(&mut tx, &env.catalog, &env.plan, &scope(&["title"]), &stop)
        .await
        .expect("a stop word is a valid term");
    assert!(
        page.hits.is_empty(),
        "a stop word matched {} rows instead of none",
        page.hits.len()
    );
    let real = SearchRequest::new(term("harbour", &["title"])).with_limit(None);
    let page = search(&mut tx, &env.catalog, &env.plan, &scope(&["title"]), &real)
        .await
        .expect("search");
    assert_eq!(
        page.hits.len(),
        1,
        "the document is findable by its real word"
    );
    tx.rollback().await.expect("rollback");
}

// --- typo tolerance ----------------------------------------------------------------------------

#[tokio::test]
async fn prepare_refuses_typo_tolerance_without_pg_trgm_and_answers_with_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let missing = db.create_extensions(&["unaccent"]).await;
    if !missing.is_empty() {
        println!("SKIPPED: the server does not offer {missing:?}");
        return;
    }
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let catalog = common::catalog_with(true);
    let plan = common::plan(true);
    common::entity_table(&db).await;

    let mut tx = common::begin(&backend, &catalog).await;
    let error = prepare(&mut tx, std::slice::from_ref(&plan))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorKind::InvalidSchema.code());
    assert!(error.explanation().contains("pg_trgm"), "{}", error);
    tx.rollback().await.expect("rollback");

    // With the extension installed, prepare and a near-miss query answer.
    let missing = db.create_extensions(&["pg_trgm"]).await;
    assert!(missing.is_empty());
    let mut tx = common::begin(&backend, &catalog).await;
    prepare(&mut tx, std::slice::from_ref(&plan))
        .await
        .expect("prepare with pg_trgm");
    let id = RecordId::new_v7();
    tx.insert(&listing(id, "harbour", None, None))
        .await
        .expect("insert");
    upsert(&mut tx, &plan, id).await.expect("project");
    tx.commit().await.expect("commit");

    let mut tx = common::begin(&backend, &catalog).await;
    let request = SearchRequest::new(term("harbur", &["title"])).with_limit(None);
    let page = search(&mut tx, &catalog, &plan, &scope(&["title"]), &request)
        .await
        .expect("a typo is answered by trigram similarity");
    assert_eq!(page.hits.len(), 1, "`harbur` finds `harbour` with pg_trgm");
    tx.rollback().await.expect("rollback");

    // The same query without typo tolerance matches nothing: the difference is the extension.
    plan_without_typo_matches_nothing(&backend).await;
}

/// A catalog that does not declare typo tolerance matches nothing for a near miss: the trigram
/// branch is the declaration's, and `pg_trgm` is what serves it.
async fn plan_without_typo_matches_nothing(backend: &PgBackend) {
    let catalog = common::catalog_with(false);
    let plan = common::plan(false);
    let mut tx = common::begin(backend, &catalog).await;
    rebuild(&mut tx, &plan).await.expect("rebuild");
    let request = SearchRequest::new(term("harbur", &["title"])).with_limit(None);
    let page = search(&mut tx, &catalog, &plan, &scope(&["title"]), &request)
        .await
        .expect("search");
    assert!(page.hits.is_empty(), "no trigram, no typo match");
    tx.rollback().await.expect("rollback");
}

// --- rebuild ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_rebuild_reproduces_the_projection_exactly() {
    let Some(env) = common::env(false).await else {
        return;
    };
    common::seed_ten_thousand(&env.db).await;
    common::project_all(&env).await;
    let first = snapshot(&env.db).await;
    common::project_all(&env).await;
    let second = snapshot(&env.db).await;
    assert_eq!(
        first, second,
        "a rebuild is identical to the projection it replaced"
    );
    assert_eq!(first.len(), 10_000);

    // A single-row write and a rebuild produce the same row: one definition of the projection.
    let client = env.db.raw_client().await;
    client
        .batch_execute(
            "UPDATE \"listing\" SET \"title\" = 'Harbour penthouse' WHERE \"id\" = \
                        '00000000-0000-7000-8000-000000000001'",
        )
        .await
        .expect("update the entity");
    let id = RecordId::from_bytes(
        *uuid::Uuid::parse_str(&common::uuid_for(1))
            .expect("a uuid")
            .as_bytes(),
    )
    .expect("a v7 id");
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    upsert(&mut tx, &env.plan, id).await.expect("one write");
    tx.commit().await.expect("commit");
    let written = snapshot(&env.db).await;
    common::project_all(&env).await;
    assert_eq!(
        snapshot(&env.db).await,
        written,
        "a rebuild equals the write"
    );
    assert_ne!(first, written, "the changed title changed the projection");
}

/// Every projection row, as text: id, terms, facets and language.
async fn snapshot(db: &TestDb) -> Vec<String> {
    let client = db.raw_client().await;
    client
        .query(
            "SELECT \"doc_id\"::text || '|' || \"terms\"::text || '|' || \"facets\"::text || '|' \
             || \"tenant_language\" FROM \"spot_search_doc\" ORDER BY \"doc_id\"",
            &[],
        )
        .await
        .expect("the projection")
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect()
}

// --- maintenance in the write's transaction ----------------------------------------------------

#[tokio::test]
async fn maintenance_rolls_back_and_commits_with_the_document() {
    let Some(env) = common::env(false).await else {
        return;
    };
    let id = RecordId::new_v7();
    let doc = listing(id, "harbour cottage", Some("ساحل"), None);

    // Rolled back: neither the document nor its projection exists.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    tx.insert(&doc).await.expect("insert");
    upsert(&mut tx, &env.plan, id).await.expect("project");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        count(&env.db).await,
        0,
        "a rollback leaves no projection row"
    );

    // Committed: both exist, and the projection was built from the stored row.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    tx.insert(&doc).await.expect("insert");
    upsert(&mut tx, &env.plan, id).await.expect("project");
    tx.commit().await.expect("commit");
    assert_eq!(count(&env.db).await, 1);
    let terms = terms_of(&env.db, id).await;
    assert!(terms.contains("harbour"), "{terms}");
    assert!(
        terms.contains("ساحل"),
        "the Urdu body is in the vector: {terms}"
    );

    // A delete takes the projection with it.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    tx.delete(&n("listing"), id, Version::FIRST)
        .await
        .expect("delete");
    remove(&mut tx, &n("listing"), id).await.expect("unproject");
    tx.commit().await.expect("commit");
    assert_eq!(count(&env.db).await, 0);
}

async fn count(db: &TestDb) -> i64 {
    common::scalar_i64(db, "SELECT count(*) FROM \"spot_search_doc\"").await
}

async fn terms_of(db: &TestDb, id: RecordId) -> String {
    let client = db.raw_client().await;
    client
        .query_one(
            "SELECT \"terms\"::text FROM \"spot_search_doc\" WHERE \"doc_id\" = $1::uuid",
            &[&uuid::Uuid::from_bytes(*id.as_bytes())],
        )
        .await
        .expect("the projection row")
        .get(0)
}

// --- facets ------------------------------------------------------------------------------------

#[tokio::test]
async fn facet_counts_obey_the_permission_filter() {
    let Some(env) = common::env(false).await else {
        return;
    };
    let ids: Vec<RecordId> = (0..4).map(|_| RecordId::new_v7()).collect();
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    for (index, id) in ids.iter().enumerate() {
        let mut doc = listing(*id, "harbour cottage", None, None);
        doc.fields.insert(
            n("price_band"),
            Value::Text(if index % 2 == 0 {
                "cheap".to_owned()
            } else {
                "pricey".to_owned()
            }),
        );
        doc.fields.insert(
            n("guests"),
            Value::Int(i64::try_from(index).unwrap_or(0) + 1),
        );
        tx.insert(&doc).await.expect("insert");
    }
    rebuild(&mut tx, &env.plan).await.expect("rebuild");
    tx.commit().await.expect("commit");

    // Every row matches: two cheap, two pricey.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term("harbour", &["title"]))
        .with_facets([n("price_band"), n("guests")])
        .with_limit(None);
    let page = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &scope(&["title", "price_band", "guests"]),
        &request,
    )
    .await
    .expect("search with facets");
    let price = page
        .facets
        .get(&n("price_band"))
        .expect("a price band facet");
    assert_eq!(price.len(), 2);
    assert_eq!(price.iter().map(|b| b.count).sum::<u64>(), 4);
    assert!(price.iter().any(|b| b.value == "cheap" && b.count == 2));
    let guests = page.facets.get(&n("guests")).expect("a guests facet");
    assert_eq!(guests.len(), 4, "one bucket per guest count");

    // The same facet counts under a row filter count only the permitted rows.
    let filter = Expr::Cmp(
        Box::new(Expr::field(&n("s"), &n("guests"))),
        CmpOp::Lt,
        Box::new(Expr::Value(Value::Int(3))),
    );
    let permitted = ReadScope::new(
        [n("title"), n("price_band"), n("guests")],
        RowScope::Filter(filter),
    );
    let page = search(&mut tx, &env.catalog, &env.plan, &permitted, &request)
        .await
        .expect("search with facets");
    assert_eq!(page.hits.len(), 2, "two rows have fewer than three guests");
    let price = page
        .facets
        .get(&n("price_band"))
        .expect("a price band facet");
    assert_eq!(
        price.iter().map(|b| b.count).sum::<u64>(),
        2,
        "the count follows the filter, never the index's totals"
    );
    tx.rollback().await.expect("rollback");

    // A facet the caller may not read is refused, not silently dropped.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term("harbour", &["title"])).with_facets([n("price_band")]);
    let error = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &scope(&["title"]),
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "permission.field_denied");
    tx.rollback().await.expect("rollback");
}

// --- conformance -------------------------------------------------------------------------------

#[tokio::test]
async fn a_non_zero_gin_fuzzy_search_limit_fails_the_conformance_check() {
    let Some(env) = common::env(false).await else {
        return;
    };
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    check(&mut tx, std::slice::from_ref(&env.plan))
        .await
        .expect("a fresh database conforms");
    tx.raw_execute_with(
        spot_postgres::PgTransaction::READ,
        "SET LOCAL \"gin_fuzzy_search_limit\" = 1000",
        &[],
    )
    .await
    .expect("the setting is settable");
    let error = check(&mut tx, std::slice::from_ref(&env.plan))
        .await
        .unwrap_err();
    assert!(
        error.explanation().contains("gin_fuzzy_search_limit"),
        "{error}"
    );
    assert!(error.explanation().contains("must be 0"), "{error}");
    tx.rollback().await.expect("rollback");
}

#[tokio::test]
async fn prepare_without_unaccent_is_refused_naming_the_extension() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let catalog = common::catalog();
    common::entity_table(&db).await;
    let mut tx = common::begin(&backend, &catalog).await;
    let error = prepare(&mut tx, std::slice::from_ref(&common::plan(false)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorKind::InvalidSchema.code());
    assert!(error.explanation().contains("unaccent"), "{error}");
    assert!(error.explanation().contains("extension.install"), "{error}");
    tx.rollback().await.expect("rollback");
}

// --- projection lag ----------------------------------------------------------------------------

#[tokio::test]
async fn projection_lag_counts_the_documents_that_are_not_projected() {
    let Some(env) = common::env(false).await else {
        return;
    };
    common::seed_ten_thousand(&env.db).await;
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let lag = measure_doctype(&mut tx, &n("listing")).await.expect("lag");
    assert_eq!(lag.missing, 10_000, "nothing is projected yet");
    assert_eq!(lag.projected, 0);
    tx.rollback().await.expect("rollback");

    common::project_all(&env).await;
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let lag = measure_doctype(&mut tx, &n("listing")).await.expect("lag");
    assert_eq!((lag.missing, lag.projected), (0, 10_000));
    assert!(lag.within_bound(spot_pg_search::DEFAULT_BOUND_MS));
    assert!(lag.age_ms.is_some(), "the newest row reports its age");
    tx.rollback().await.expect("rollback");

    // A stale projection past the bound refuses the read rather than answering as if current.
    let client = env.db.raw_client().await;
    client
        .batch_execute(
            "UPDATE \"spot_search_doc\" SET \"updated_at\" = now() - interval '10 minutes'",
        )
        .await
        .expect("age the projection");
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term("harbour", &["title"]))
        .with_limit(Some(5))
        .with_lag_bound(1_000);
    let error = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &scope(&["title"]),
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), ErrorKind::Unavailable.code());
    tx.rollback().await.expect("rollback");
}
