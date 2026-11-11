#![cfg(feature = "testing")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_postgres::{PgConfig, create_database, database_exists, testing::TestDb};
use spot_storage_contract::ErrorKind;

#[tokio::test]
async fn rejects_hostile_names_before_connecting() {
    let offline = PgConfig::new("127.0.0.1", 1, "postgres", "unused", "unused");
    for name in [
        "",
        "UPPER",
        "a-b",
        "a\"; DROP DATABASE postgres;--",
        "a.b",
        "é",
        &"a".repeat(64),
    ] {
        assert_eq!(
            database_exists(&offline, name).await.unwrap_err().kind,
            ErrorKind::InvalidValue
        );
        assert_eq!(
            create_database(&offline, name, "en")
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidValue
        );
    }
    for locale in ["", "en'", "en;SELECT 1", "en US", "en\\US"] {
        assert_eq!(
            create_database(&offline, "safe", locale)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidValue
        );
    }
}

#[tokio::test]
async fn creates_icu_database_and_refuses_duplicate_without_altering_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let admin = db.config();
    let name = format!("{}_admin", db.name());
    assert!(!database_exists(&admin, &name).await.unwrap());
    create_database(&admin, &name, "en").await.unwrap();
    assert!(database_exists(&admin, &name).await.unwrap());
    let client = db.admin_client().await;
    let before = client.query_one("SELECT oid, datlocprovider::text, datlocale, pg_encoding_to_char(encoding) FROM pg_database WHERE datname = $1", &[&name]).await.unwrap();
    assert_eq!(before.get::<_, String>(1), "i");
    assert_eq!(before.get::<_, String>(2), "en");
    assert_eq!(before.get::<_, String>(3), "UTF8");
    assert!(create_database(&admin, &name, "und").await.is_err());
    let after = client
        .query_one(
            "SELECT oid, datlocale FROM pg_database WHERE datname = $1",
            &[&name],
        )
        .await
        .unwrap();
    assert_eq!(before.get::<_, u32>(0), after.get::<_, u32>(0));
    assert_eq!(after.get::<_, String>(1), "en");
    // Test-owned resource only, not a provisioning operation.
    client
        .batch_execute(&format!("DROP DATABASE \"{name}\""))
        .await
        .unwrap();
}
