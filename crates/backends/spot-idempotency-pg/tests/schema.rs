//! The table: `spot_` prefixed and accepted by the migrator, the primary key and checks refuse
//! what the store never writes, `prepare` is idempotent and safe from several nodes at once, and
//! what the store writes holds no secret.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{clock, config, fp, pool, response, row_count, scope, store, store_on};
use spot_http::api::{IdempotencyStore, KeyScope, Reservation, StoredResponse};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

/// Every relation, index and constraint name of the current schema, sorted.
async fn object_names(db: &TestDb) -> Vec<String> {
    db.raw_client()
        .await
        .query(
            "SELECT 'rel:' || relname::text FROM pg_class \
             WHERE relnamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'con:' || conname::text FROM pg_constraint \
             WHERE connamespace = to_regnamespace(current_schema())::oid \
             UNION SELECT 'idx:' || c.relname::text FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relnamespace = to_regnamespace(current_schema())::oid ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn migrator_refusals(db: &TestDb) -> usize {
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    plan(&live, &wanted).refused.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_object_the_crate_creates_is_spot_prefixed_and_passes_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let _store = store(&db, 2, "idem_schema", &clock).await;
    let tables: Vec<String> = db
        .raw_client()
        .await
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(tables, vec!["spot_idempotency_key".to_owned()]);
    let names = object_names(&db).await;
    for name in &names {
        let bare = name.split_once(':').unwrap().1;
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
    for wanted in [
        "con:spot_idempotency_key_pkey",
        "idx:spot_idempotency_key_expires_idx",
        "idx:spot_idempotency_key_lease_idx",
    ] {
        assert!(names.iter().any(|n| n == wanted), "missing {wanted}");
    }
    assert_eq!(
        migrator_refusals(&db).await,
        0,
        "the migrator accepts the table"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_safe_from_several_nodes_at_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let stores: Vec<_> = (0..4)
        .map(|n| {
            spot_idempotency_pg::PgIdempotencyStore::new(
                pool(&db, 2, &format!("idem_prepare_{n}")),
                clock.clone(),
                config(),
            )
            .unwrap()
        })
        .collect();
    let results = futures_join(&stores).await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    let before = object_names(&db).await;
    stores[0].prepare().await.unwrap();
    assert_eq!(
        object_names(&db).await,
        before,
        "a second prepare changes nothing"
    );
}

async fn futures_join(
    stores: &[spot_idempotency_pg::PgIdempotencyStore],
) -> Vec<Result<(), String>> {
    let mut tasks = tokio::task::JoinSet::new();
    for store in stores {
        let store = store.clone();
        tasks.spawn(async move { store.prepare().await.map_err(|e| e.to_string()) });
    }
    let mut out = Vec::new();
    while let Some(done) = tasks.join_next().await {
        out.push(done.unwrap());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_primary_key_refuses_a_second_row_for_one_scope() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store(&db, 2, "idem_pk", &clock).await;
    let sc = scope("cred", "pk-1");
    assert_eq!(s.reserve(&sc, &fp("a")).await, Ok(Reservation::Fresh));
    let raw = db.raw_client().await;
    let err = raw
        .execute(
            "INSERT INTO spot_idempotency_key \
             (site, credential, key, fingerprint, state, lease_expires_at, created_at) \
             VALUES ($1, $2, $3, $4, 'pending', 1, 1)",
            &[&sc.site, &sc.credential, &sc.key, &fp("a")],
        )
        .await
        .unwrap_err();
    assert_eq!(err.as_db_error().unwrap().code().code(), "23505");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_checks_refuse_rows_the_store_never_writes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let _s = store(&db, 2, "idem_checks", &clock).await;
    let raw = db.raw_client().await;
    let good = fp("a");
    let insert = |key: &'static str, fingerprint: String, state: &'static str| {
        let raw = &raw;
        async move {
            raw.execute(
                &format!(
                    "INSERT INTO spot_idempotency_key \
                     (site, credential, key, fingerprint, state, lease_expires_at, created_at) \
                     VALUES ('acme', 'c', $1, $2, '{state}', 1, 1)"
                ),
                &[&key, &fingerprint],
            )
            .await
        }
    };
    for (what, outcome) in [
        (
            "a key with a space",
            insert("has space", good.clone(), "pending").await,
        ),
        ("an empty key", insert("", good.clone(), "pending").await),
        (
            "a fingerprint that is not hex",
            insert("k", "xyz".to_owned(), "pending").await,
        ),
        (
            "an upper-case fingerprint",
            insert("k", good.to_uppercase(), "pending").await,
        ),
        ("an unknown state", insert("k", good.clone(), "weird").await),
        (
            "a done row with no response",
            insert("k", good.clone(), "done").await,
        ),
    ] {
        assert_eq!(
            outcome.unwrap_err().as_db_error().unwrap().code().code(),
            "23514",
            "{what} must violate a check"
        );
    }
    let long = "k".repeat(129);
    let err = raw
        .execute(
            "INSERT INTO spot_idempotency_key \
             (site, credential, key, fingerprint, state, lease_expires_at, created_at) \
             VALUES ('acme', 'c', $1, $2, 'pending', 1, 1)",
            &[&long, &good],
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code().code(),
        "23514",
        "a 129-byte key"
    );
    let huge = vec![0u8; 1_048_577];
    let err = raw
        .execute(
            "INSERT INTO spot_idempotency_key \
             (site, credential, key, fingerprint, state, expires_at, status, headers, body, created_at) \
             VALUES ('acme', 'c', 'big', $1, 'done', 9, 200, '[]'::jsonb, $2, 1)",
            &[&good, &huge],
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code().code(),
        "23514",
        "a body over 1 MiB is refused by the table itself"
    );
    assert_eq!(row_count(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_allow_listed_headers_and_the_exact_body_are_written() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store(&db, 2, "idem_headers", &clock).await;
    let sc = scope("hash-of-token", "headers-1");
    s.reserve(&sc, &fp("a")).await.unwrap();
    let body = br#"{"data":{"id":"1","phone":"********"}}"#;
    s.complete(
        &sc,
        &fp("a"),
        StoredResponse {
            status: 201,
            headers: [
                ("Content-Type", "application/json"),
                ("ETag", "\"1\""),
                ("Location", "/api/v1/resource/x/1"),
                ("Retry-After", "5"),
                ("Set-Cookie", "spot_session=SECRET"),
                ("Authorization", "Bearer SECRET"),
                ("Idempotency-Replayed", "true"),
                ("X-Api-Key", "SECRET"),
            ]
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
            body: body.to_vec(),
        },
        "http_api",
    )
    .await
    .unwrap();
    let raw = db.raw_client().await;
    let row = raw
        .query_one(
            "SELECT headers::text, body, credential FROM spot_idempotency_key WHERE key = 'headers-1'",
            &[],
        )
        .await
        .unwrap();
    let headers: String = row.get(0);
    let stored_body: Vec<u8> = row.get(1);
    let credential: String = row.get(2);
    assert!(!headers.to_lowercase().contains("secret"), "{headers}");
    for kept in ["Content-Type", "ETag", "Location", "Retry-After"] {
        assert!(headers.contains(kept), "{kept} kept: {headers}");
    }
    for dropped in [
        "Set-Cookie",
        "Authorization",
        "Idempotency-Replayed",
        "X-Api-Key",
    ] {
        assert!(!headers.contains(dropped), "{dropped} dropped: {headers}");
    }
    assert_eq!(
        stored_body,
        body.to_vec(),
        "the body is stored as given, masked by the caller"
    );
    assert_eq!(
        credential, "hash-of-token",
        "only the credential hash the router passed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_over_the_cap_is_released_not_stored() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store_on(
        pool(&db, 2, "idem_cap"),
        &clock,
        config().with_max_body_bytes(16),
    )
    .await;
    assert_eq!(s.max_body_bytes(), 16);
    let sc = scope("cred", "cap-1");
    s.reserve(&sc, &fp("a")).await.unwrap();
    s.complete(&sc, &fp("a"), response(200, &[b'x'; 17]), "http_api")
        .await
        .unwrap();
    assert_eq!(
        row_count(&db).await,
        0,
        "nothing written, the pending row is gone"
    );
    assert_eq!(s.reserve(&sc, &fp("a")).await, Ok(Reservation::Fresh));
    s.complete(&sc, &fp("a"), response(200, &[b'x'; 16]), "http_api")
        .await
        .unwrap();
    assert!(matches!(
        s.reserve(&sc, &fp("a")).await,
        Ok(Reservation::Completed(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fail_leaves_no_row_and_never_touches_a_stored_response() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store(&db, 2, "idem_fail", &clock).await;
    let sc = scope("cred", "fail-1");
    s.reserve(&sc, &fp("a")).await.unwrap();
    s.fail(&sc, &fp("a")).await;
    assert_eq!(row_count(&db).await, 0);
    s.reserve(&sc, &fp("a")).await.unwrap();
    s.complete(&sc, &fp("a"), response(200, b"{}"), "http_api")
        .await
        .unwrap();
    s.fail(&sc, &fp("a")).await;
    assert_eq!(
        row_count(&db).await,
        1,
        "a stored response survives a late fail"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_values_are_data_never_sql() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store(&db, 2, "idem_inject", &clock).await;
    // Printable ASCII, no spaces: a valid key.
    let key = "a';DROP/**/TABLE/**/spot_idempotency_key;--";
    let sc = KeyScope {
        site: "ac'me\"; DROP TABLE spot_idempotency_key;--".to_owned(),
        credential: "c' OR '1'='1".to_owned(),
        key: key.to_owned(),
    };
    assert_eq!(s.reserve(&sc, &fp("a")).await, Ok(Reservation::Fresh));
    s.complete(
        &sc,
        &fp("a"),
        response(200, b"{}"),
        "x'; DELETE FROM spot_idempotency_key;--",
    )
    .await
    .unwrap();
    assert!(matches!(
        s.reserve(&sc, &fp("a")).await,
        Ok(Reservation::Completed(_))
    ));
    assert_eq!(row_count(&db).await, 1, "the table is intact");
    // Values the table would refuse are refused before any SQL, as unavailable.
    for bad in [
        KeyScope {
            key: String::new(),
            ..sc.clone()
        },
        KeyScope {
            key: "has space".to_owned(),
            ..sc.clone()
        },
        KeyScope {
            key: "k".repeat(129),
            ..sc.clone()
        },
        KeyScope {
            credential: String::new(),
            ..sc.clone()
        },
    ] {
        assert!(s.reserve(&bad, &fp("a")).await.is_err());
    }
    assert!(s.reserve(&sc, "not-a-fingerprint").await.is_err());
    // A key that differs only in case is another key.
    let other = KeyScope {
        key: key.to_uppercase(),
        ..sc.clone()
    };
    assert_eq!(s.reserve(&other, &fp("a")).await, Ok(Reservation::Fresh));
}
