//! Accent folding and the required extensions (ADR-087 section 7): a query typed without the
//! accent finds the accented text, a protected field's text is never findable, a typo finds the
//! word when `pg_trgm` is on, and a server without `unaccent` or `pg_trgm` fails the CI run
//! (`SPOT_REQUIRE_PG=1`) instead of skipping.
//!
//! Every title here is projected with `simple` (no stemming), so the accent is the only thing
//! between the query and the stored lexeme and the crate's search and `Expr::Search` agree.
//!
//! The reverse direction (a query typed *with* the accent) works because `spot-pg-query` folds
//! the bound query text with the same `unaccent` call as the projection.
//!
//! ```text
//! SPOT_TEST_PG_URL=postgres://user:pass@localhost:5432/postgres SPOT_REQUIRE_PG=1 \
//!   cargo test -p spot-pg-search --test unaccent
//! ```
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Env, ExtensionCheck, judge_extensions, n};
use spot_pg_search::{Language, ReadScope, SearchRequest, rebuild, search};
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    DocumentData, DocumentStore, Fields, RecordId, SearchMode, SearchTerm, Transaction, Value,
};

fn term(text: &str, fields: &[&str]) -> SearchTerm {
    SearchTerm {
        mode: SearchMode::Web,
        text: text.to_owned(),
        fields: fields.iter().map(|f| n(f)).collect(),
    }
}

fn both_fields() -> ReadScope {
    ReadScope::all_rows(["title", "body"].iter().map(|f| n(f)))
}

/// A `listing` document: a title, an optional body and an optional protected note.
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

/// The environment with the title projected as `simple` too (the body already is).
async fn simple_env(typo: bool) -> Option<Env> {
    let plan = common::plan(typo)
        .with_field_language(&n("title"), Language::new("ur").unwrap())
        .unwrap();
    common::env_with(typo, plan).await
}

/// Inserts the documents and projects them.
async fn load(env: &Env, docs: &[DocumentData]) {
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    for doc in docs {
        tx.insert(doc).await.expect("insert");
    }
    rebuild(&mut tx, &env.plan).await.expect("rebuild");
    tx.commit().await.expect("commit");
}

/// The ids a query finds, in rank order.
async fn find(env: &Env, text: &str) -> Vec<RecordId> {
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term(text, &["title", "body"])).with_limit(None);
    let page = search(&mut tx, &env.catalog, &env.plan, &both_fields(), &request)
        .await
        .expect("search");
    tx.rollback().await.expect("rollback");
    page.hits.iter().map(|h| h.id).collect()
}

// --- the server -------------------------------------------------------------------------------

#[tokio::test]
async fn the_required_extensions_are_on_the_test_server() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Under SPOT_REQUIRE_PG=1 this panics, naming spot-pg-search, when either is missing.
    if !common::require_extensions(&db).await {
        return;
    }
    let client = db.raw_client().await;
    let folded: String = client
        .query_one(
            "SELECT unaccent('unaccent'::regdictionary, 'Café Straße naïve Lāhore')",
            &[],
        )
        .await
        .expect("unaccent")
        .get(0);
    assert_eq!(folded, "Cafe Strasse naive Lahore");
    let similarity: f32 = client
        .query_one("SELECT similarity('harbour', 'harbur')", &[])
        .await
        .expect("pg_trgm")
        .get(0);
    assert!(similarity > 0.3, "pg_trgm answers: {similarity}");
}

#[test]
fn a_missing_extension_fails_when_required_and_skips_when_not() {
    let none: Vec<String> = Vec::new();
    assert_eq!(judge_extensions(&none, true), ExtensionCheck::Present);
    assert_eq!(judge_extensions(&none, false), ExtensionCheck::Present);

    let missing = vec!["unaccent".to_owned()];
    match judge_extensions(&missing, true) {
        ExtensionCheck::Fail(message) => {
            assert!(message.contains("spot-pg-search"), "{message}");
            assert!(message.contains("unaccent"), "{message}");
        }
        other => panic!("a required, missing extension must fail, got {other:?}"),
    }
    let missing = vec!["pg_trgm".to_owned()];
    match judge_extensions(&missing, false) {
        ExtensionCheck::Skip(message) => {
            assert!(message.starts_with("SKIPPED:"), "{message}");
            assert!(message.contains("pg_trgm"), "{message}");
        }
        other => panic!("an unrequired, missing extension must skip, got {other:?}"),
    }
}

// --- accent folding ---------------------------------------------------------------------------

#[tokio::test]
async fn a_query_without_the_accent_finds_the_accented_text() {
    let Some(env) = simple_env(false).await else {
        return;
    };
    let (cafe, strasse, naive, lahore, other) = (
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
    );
    load(
        &env,
        &[
            listing(cafe, "Café du Port", None, None),
            listing(strasse, "Hotel Straße 12", None, None),
            listing(naive, "A naïve harbour", None, None),
            // Urdu transliterated in Latin script with a macron, in the body.
            listing(lahore, "Old city", Some("Lāhore ka bazaar"), None),
            listing(other, "Quiet street", Some("plain body"), None),
        ],
    )
    .await;

    for (query, expected) in [
        ("cafe", cafe),
        ("strasse", strasse),
        ("naive", naive),
        ("lahore", lahore),
    ] {
        assert_eq!(
            find(&env, query).await,
            vec![expected],
            "`{query}` finds the accented document and nothing else (the non-match is absent)"
        );
    }
    assert!(
        find(&env, "zzzz").await.is_empty(),
        "a word nobody wrote matches nothing"
    );
}

#[tokio::test]
async fn an_accented_match_ranks_above_a_weaker_match_and_a_non_match_is_absent() {
    let Some(env) = simple_env(false).await else {
        return;
    };
    let (strong, weak, none) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    load(
        &env,
        &[
            // The accented word in the weight-A title.
            listing(strong, "Café Noir", None, None),
            // The accented word only in the weight-D body.
            listing(weak, "Plain rooms", Some("un café au coin"), None),
            listing(none, "Quiet street", Some("plain body"), None),
        ],
    )
    .await;
    assert_eq!(
        find(&env, "cafe").await,
        vec![strong, weak],
        "title above body, and the non-match is not returned"
    );
}

/// A query typed with the accent finds the same documents: `spot-pg-query` folds the bound query
/// text with `unaccent('unaccent'::regdictionary, $n)`, as the projection does.
#[tokio::test]
async fn a_query_with_the_accent_finds_text_with_and_without_it() {
    let Some(env) = simple_env(false).await else {
        return;
    };
    let (accented, plain, other) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    load(
        &env,
        &[
            listing(accented, "Café du Port", None, None),
            listing(plain, "Cafe Noir", None, None),
            listing(other, "Quiet street", None, None),
        ],
    )
    .await;
    let mut found = find(&env, "café").await;
    found.sort();
    let mut expected = vec![accented, plain];
    expected.sort();
    assert_eq!(found, expected, "the accented query finds both spellings");
}

// --- protected fields -------------------------------------------------------------------------

#[tokio::test]
async fn a_protected_fields_text_is_not_findable_with_or_without_the_accent() {
    let Some(env) = simple_env(false).await else {
        return;
    };
    let id = RecordId::new_v7();
    load(
        &env,
        &[listing(id, "Quiet street", None, Some("Café privé secret"))],
    )
    .await;

    // The row is projected, and its protected column is not in the vector.
    let rows = common::scalar_i64(&env.db, "SELECT count(*) FROM \"spot_search_doc\"").await;
    assert_eq!(rows, 1);
    for lexeme in ["cafe", "café", "prive", "privé", "secret"] {
        let sql = format!(
            "SELECT count(*) FROM \"spot_search_doc\" \
             WHERE terms @@ plainto_tsquery('simple'::regconfig, '{lexeme}')"
        );
        assert_eq!(
            common::scalar_i64(&env.db, &sql).await,
            0,
            "`{lexeme}` from the protected column is in the projection"
        );
    }
    for text in ["cafe", "prive", "secret"] {
        assert!(
            find(&env, text).await.is_empty(),
            "`{text}` finds the protected note"
        );
    }

    // Naming the protected field is refused, not narrowed.
    let mut tx = common::begin(&env.backend, &env.catalog).await;
    let request = SearchRequest::new(term("cafe", &["owner_note"]));
    let error = search(
        &mut tx,
        &env.catalog,
        &env.plan,
        &ReadScope::all_rows(["title", "body"].iter().map(|f| n(f))),
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "permission.field_denied");
    tx.rollback().await.expect("rollback");
}

// --- typo tolerance ---------------------------------------------------------------------------

#[tokio::test]
async fn a_typo_finds_an_accented_word_only_when_typo_tolerance_and_pg_trgm_are_on() {
    let docs = |a: RecordId, b: RecordId| {
        [
            listing(a, "Naïve", None, None),
            listing(b, "Quiet street", None, None),
        ]
    };

    // On: `naiv` is one letter short of the folded `naive`; trigram similarity finds it.
    let Some(env) = simple_env(true).await else {
        return;
    };
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    load(&env, &docs(a, b)).await;
    assert_eq!(find(&env, "naive").await, vec![a], "the exact word");
    assert_eq!(find(&env, "naiv").await, vec![a], "the typo, pg_trgm on");

    // Off: the same data and the same typo find nothing; the difference is the declaration.
    let Some(env) = simple_env(false).await else {
        return;
    };
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    load(&env, &docs(a, b)).await;
    assert_eq!(find(&env, "naive").await, vec![a]);
    assert!(
        find(&env, "naiv").await.is_empty(),
        "no typo tolerance declared, no trigram match"
    );
}
