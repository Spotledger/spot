//! The `ProviderStore` contract suite and the row rules on a real database (WP-IAM-05.T9).
//!
//! The database cases skip cleanly without `SPOT_TEST_PG_URL` (and fail with `SPOT_REQUIRE_PG=1`,
//! which CI sets). The fail-closed case needs no server.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{dead_pool, pool};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_sso::contract_stores::{run_providers, sample_provider};
use spot_sso::{Audience, ProviderId, ProviderStore, ProviderStoreError};
use spot_sso_pg::PgProviderStore;
use spot_storage_contract::{Catalog, Collation};

fn store(pool: &deadpool_postgres::Pool, tenant: &str) -> PgProviderStore {
    PgProviderStore::new(pool.clone(), tenant).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4, "sso_provider_contract");
    store(&pool, "prepare").prepare().await.unwrap();
    let next = AtomicUsize::new(0);
    // Like the replay suite, it is synchronous: each future is driven on the calling worker.
    run_providers(|| {
        let n = next.fetch_add(1, Ordering::SeqCst);
        (
            store(&pool, &format!("tenant-a-{n}")),
            store(&pool, &format!("tenant-b-{n}")),
        )
    });
}

/// The stored row holds the audience and only the name of the client secret.
#[tokio::test]
async fn the_row_holds_the_audience_and_the_secret_name_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_provider_row");
    let store = store(&pool, "t");
    store.prepare().await.unwrap();
    store
        .put(&sample_provider("market", Audience::Consumer))
        .await
        .unwrap();
    let client = db.raw_client().await;
    let row = client
        .query_one(
            "SELECT audience, config FROM spot_sso_provider WHERE tenant = 't' AND id = 'market'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "consumer");
    let config: String = row.get(1);
    assert!(config.contains("providers/market/client-secret"));
    assert!(config.contains("\"audience\":\"consumer\""));
}

/// A row that no longer validates is skipped and reported, not served.
#[tokio::test]
async fn an_invalid_stored_row_is_skipped_and_reported() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_provider_skip");
    let store = store(&pool, "t");
    store.prepare().await.unwrap();
    store
        .put(&sample_provider("good", Audience::Staff))
        .await
        .unwrap();
    let mut broken = sample_provider("broken", Audience::Staff);
    broken.issuer = "http://idp.example.com".to_owned();
    let text = serde_json::to_string(&broken).unwrap();
    let client = db.raw_client().await;
    client
        .execute(
            "INSERT INTO spot_sso_provider (tenant, id, audience, config) VALUES ('t', 'broken', 'staff', $1)",
            &[&text],
        )
        .await
        .unwrap();
    // A row whose audience column disagrees with its configuration is not served either.
    let mismatch = serde_json::to_string(&sample_provider("odd", Audience::Staff)).unwrap();
    client
        .execute(
            "INSERT INTO spot_sso_provider (tenant, id, audience, config) VALUES ('t', 'odd', 'consumer', $1)",
            &[&mismatch],
        )
        .await
        .unwrap();
    let listing = store.list().await.unwrap();
    assert_eq!(listing.providers.len(), 1);
    assert_eq!(
        listing.skipped,
        vec![ProviderId::new("broken"), ProviderId::new("odd")]
    );
    for id in ["broken", "odd"] {
        assert!(matches!(
            store.get(&ProviderId::new(id)).await,
            Err(ProviderStoreError::Invalid(_))
        ));
    }
}

/// A pasted secret in place of a reference never reaches the table.
#[tokio::test]
async fn a_pasted_secret_is_refused_and_not_stored() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_provider_secret");
    let store = store(&pool, "t");
    store.prepare().await.unwrap();
    let mut config = sample_provider("acme", Audience::Staff);
    config.auth = spot_sso::ClientAuth::ClientSecretRef {
        secret_ref: "c2VjcmV0+value==".to_owned(),
    };
    assert!(matches!(
        store.put(&config).await,
        Err(ProviderStoreError::Invalid(_))
    ));
    let client = db.raw_client().await;
    let count: i64 = client
        .query_one("SELECT count(*) FROM spot_sso_provider", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

/// Every object the three stores create carries the `spot_` prefix and passes the migrator.
#[tokio::test]
async fn the_tables_are_spot_prefixed_and_pass_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_provider_migrator");
    store(&pool, "t").prepare().await.unwrap();
    let pending =
        spot_sso_pg::PgPendingStore::new(pool.clone(), "t", &[1_u8; 32], common::sealer(1))
            .unwrap();
    pending.prepare().await.unwrap();
    let client = db.raw_client().await;
    let names: Vec<String> = client
        .query(
            "SELECT relname::text FROM pg_class \
             WHERE relnamespace = to_regnamespace(current_schema())::oid ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(names.iter().any(|n| n == "spot_sso_provider"));
    assert!(names.iter().any(|n| n == "spot_sso_pending"));
    for name in &names {
        assert!(name.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    assert_eq!(plan(&live, &wanted).refused.len(), 0);
}

#[tokio::test]
async fn an_unreachable_database_is_unavailable_never_empty() {
    let store = PgProviderStore::new(dead_pool(), "t").unwrap();
    assert_eq!(store.list().await, Err(ProviderStoreError::Unavailable));
    assert_eq!(
        store.get(&ProviderId::new("a")).await,
        Err(ProviderStoreError::Unavailable)
    );
    assert_eq!(
        store.delete(&ProviderId::new("a")).await,
        Err(ProviderStoreError::Unavailable)
    );
    assert_eq!(
        store.put(&sample_provider("a", Audience::Staff)).await,
        Err(ProviderStoreError::Unavailable)
    );
}
