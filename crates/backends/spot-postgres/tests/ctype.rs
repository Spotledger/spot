//! The ctype of tenant databases (WP-STO-13.T4): `create_database` and `TestDb` use a Unicode
//! ctype, so `pg_trgm` builds trigrams for Urdu, and `check_text_search_ctype` proves it.
//!
//! Gated on `SPOT_TEST_PG_URL` like the neighbours; `SPOT_REQUIRE_PG=1` makes a missing server a
//! failure.
#![cfg(feature = "testing")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_postgres::testing::TestDb;
use spot_postgres::{
    DEFAULT_CTYPE, PgConfig, check_text_search_ctype, create_database, create_database_with_ctype,
    database_exists,
};
use spot_storage_contract::ErrorKind;

const URDU: &str = "\u{627}\u{631}\u{62f}\u{648}";

async fn trigram_count(client: &tokio_postgres::Client, word: &str) -> i32 {
    client
        .query_one("SELECT cardinality(show_trgm($1::text))", &[&word])
        .await
        .unwrap()
        .get(0)
}

async fn drop_database(db: &TestDb, name: &str) {
    db.admin_client()
        .await
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_invalid_ctype_is_refused_before_any_sql_is_sent() {
    let offline = PgConfig::new("127.0.0.1", 1, "postgres", "unused", "unused");
    for ctype in [
        "",
        "C.UTF-8'",
        "C.UTF-8; SELECT 1",
        "en US",
        "en\\US",
        "C.UTF-8\"",
        "\u{e9}",
    ] {
        let error = create_database_with_ctype(&offline, "safe", "und", ctype)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidValue, "ctype {ctype:?}");
    }
    // The default is a valid token, so it passes validation and then fails to connect.
    let error = create_database(&offline, "safe", "und").await.unwrap_err();
    assert_ne!(error.kind, ErrorKind::InvalidValue);
    assert_eq!(DEFAULT_CTYPE, "C.UTF-8");
}

#[tokio::test]
async fn a_database_from_create_database_passes_the_check() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let admin = db.config();
    let name = format!("{}_ctype", db.name());
    create_database(&admin, &name, "und").await.unwrap();
    let tenant = admin.clone().with_dbname(&name);
    let client = db.raw_client_on(&tenant).await;
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .await
        .unwrap();
    let datctype: String = db
        .admin_client()
        .await
        .query_one(
            "SELECT datctype::text FROM pg_database WHERE datname = $1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(datctype, DEFAULT_CTYPE);
    assert!(trigram_count(&client, URDU).await > 0);
    check_text_search_ctype(&tenant).await.unwrap();
    drop(client);
    drop_database(&db, &name).await;
}

#[tokio::test]
async fn a_database_with_a_c_ctype_fails_the_check_naming_the_ctype() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let name = format!("{}_cctype", db.name());
    db.admin_client()
        .await
        .batch_execute(&format!(
            "CREATE DATABASE \"{name}\" TEMPLATE template0 ENCODING 'UTF8' \
             LOCALE 'C' LOCALE_PROVIDER icu ICU_LOCALE 'und'"
        ))
        .await
        .unwrap();
    let tenant = db.config().with_dbname(&name);
    let client = db.raw_client_on(&tenant).await;
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .await
        .unwrap();
    assert_eq!(trigram_count(&client, URDU).await, 0);
    let error = check_text_search_ctype(&tenant).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(error.explanation.contains("show_trgm"), "{error:?}");
    assert!(error.explanation.contains("datctype `C`"), "{error:?}");
    assert!(!error.explanation.contains("postgres://"), "{error:?}");
    drop(client);
    drop_database(&db, &name).await;
}

#[tokio::test]
async fn the_check_does_not_install_the_extension() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // `TestDb` creates no extension by itself.
    let error = check_text_search_ctype(&db.config()).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidSchema);
    assert!(error.explanation.contains("pg_trgm"), "{error:?}");
    let installed = db
        .raw_client()
        .await
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
            &[],
        )
        .await
        .unwrap()
        .get::<_, bool>(0);
    assert!(!installed, "the check must not install pg_trgm");
}

#[tokio::test]
async fn a_missing_locale_fails_clearly_and_creates_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let admin = db.config();
    let name = format!("{}_nolocale", db.name());
    let error = create_database_with_ctype(&admin, &name, "und", "xx_XX.UTF-8")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error:?}");
    assert!(error.explanation.contains("xx_XX.UTF-8"), "{error:?}");
    assert!(!database_exists(&admin, &name).await.unwrap());
}

#[tokio::test]
async fn a_test_database_builds_trigrams_for_urdu_and_english() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let missing = db.require_extensions(&["pg_trgm"]).await;
    assert!(missing.is_empty(), "pg_trgm is not offered: {missing:?}");
    let client = db.raw_client().await;
    assert!(
        trigram_count(&client, URDU).await > 0,
        "Urdu has no trigrams"
    );
    assert!(trigram_count(&client, "invoice").await > 0);
    check_text_search_ctype(&db.config()).await.unwrap();
}
