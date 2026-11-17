//! The settings store: the in-memory and the PostgreSQL implementation run the same cases. The
//! PostgreSQL ones need `SPOT_TEST_PG_URL` and print the reason when they skip.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;

use spot_mgmt_ops::ParamValue;
use spot_settings::{ScopeRef, SecretValue, StoredValue};
use spot_settings_store_pg::{MemorySettingsStore, SetRequest, SettingsBackend, VERSION_CONFLICT};

fn request(key: &str, value: Option<StoredValue>, expected: Option<u64>, at: i64) -> SetRequest {
    SetRequest {
        scope: ScopeRef::Tenant("acme".to_owned()),
        key: key.to_owned(),
        value,
        expected,
        actor: "alice".to_owned(),
        channel: "mgmt".to_owned(),
        at,
    }
}

fn int(n: i64) -> Option<StoredValue> {
    Some(StoredValue::Value(ParamValue::Int(n)))
}

async fn cases(store: Arc<dyn SettingsBackend>) {
    let scope = ScopeRef::Tenant("acme".to_owned());
    // set / get / history round trip
    assert_eq!(
        store
            .set(&request("a.b", int(1), Some(0), 10))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .set(&request("a.b", int(2), Some(1), 20))
            .await
            .unwrap(),
        2
    );
    let got = store.get(&scope, "a.b").await.unwrap().unwrap();
    assert_eq!(
        (got.version, got.value),
        (2, StoredValue::Value(ParamValue::Int(2)))
    );
    let history = store.history(&scope, "a.b", 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        (history[0].version, history[0].at, history[0].actor.as_str()),
        (2, 20, "alice")
    );
    assert_eq!(history[0].old_digest, history[1].new_digest);
    assert!(history[1].old_digest.is_none());
    // a stale writer loses cleanly and nothing is written
    let stale = store
        .set(&request("a.b", int(9), Some(1), 30))
        .await
        .unwrap_err();
    assert_eq!(stale.code, VERSION_CONFLICT);
    assert_eq!(store.history(&scope, "a.b", 10).await.unwrap().len(), 2);
    // concurrent sets with one expected version: exactly one wins
    let (a, b) = (store.clone(), store.clone());
    let one = tokio::spawn(async move { a.set(&request("c.d", int(1), Some(0), 1)).await });
    let two = tokio::spawn(async move { b.set(&request("c.d", int(2), Some(0), 2)).await });
    let (one, two) = (one.await.unwrap(), two.await.unwrap());
    assert_eq!(u8::from(one.is_ok()) + u8::from(two.is_ok()), 1);
    // removal keeps counting: a recreated value never matches a stale version
    assert_eq!(
        store.set(&request("a.b", None, Some(2), 40)).await.unwrap(),
        3
    );
    assert!(store.get(&scope, "a.b").await.unwrap().is_none());
    assert_eq!(
        store
            .set(&request("a.b", int(5), Some(0), 50))
            .await
            .unwrap(),
        4
    );
    // a secret is a reference name; the history holds digests only
    let secret = Some(StoredValue::Secret(SecretValue::new("smtp-password-ref")));
    store
        .set(&request("mail.password", secret, None, 60))
        .await
        .unwrap();
    let stored = store.get(&scope, "mail.password").await.unwrap().unwrap();
    assert!(matches!(&stored.value, StoredValue::Secret(r) if r.expose() == "smtp-password-ref"));
    let rows = store.history(&scope, "mail.password", 10).await.unwrap();
    assert!(!format!("{rows:?}").contains("smtp-password-ref"));
    // list by scope; another scope sees nothing
    assert_eq!(store.list(&scope).await.unwrap().len(), 3);
    assert!(
        store
            .list(&ScopeRef::Tenant("beta".to_owned()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_memory_store_passes_the_cases() {
    cases(Arc::new(MemorySettingsStore::new())).await;
}

#[tokio::test]
async fn the_postgres_store_passes_the_same_cases() {
    let Some(db) = spot_postgres::testing::TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    let pool = cfg
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();
    let store = spot_settings_store_pg::PgSettingsStore::new(pool);
    store.prepare().await.unwrap();
    cases(Arc::new(store)).await;
}
