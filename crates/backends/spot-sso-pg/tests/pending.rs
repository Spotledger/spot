//! The `PendingStore` contract suite, restart persistence, sealing, prune and fail-closed on a
//! real database (WP-IAM-05.T9).
//!
//! The database cases skip cleanly (a `SKIPPED:` line from `TestDb::create`) when
//! `SPOT_TEST_PG_URL` is unset; with `SPOT_REQUIRE_PG=1` a missing server fails, and CI sets both
//! so they run and cannot skip. The fail-closed case needs no server.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{WIDE_POOL, dead_pool, pool, sealer, warm};
use spot_postgres::testing::TestDb;
use spot_sso::contract_stores::{run_pending, sample_pending};
use spot_sso::{PendingStore, SsoError};
use spot_sso_pg::PgPendingStore;

const KEY: [u8; 32] = [9; 32];

fn store(pool: &deadpool_postgres::Pool, tenant: &str) -> PgPendingStore {
    PgPendingStore::new(pool.clone(), tenant, &KEY, sealer(3)).unwrap()
}

/// Both tenants of one `make()` share the table and are fresh (new tenant names).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, WIDE_POOL, "sso_pending_contract");
    store(&pool, "prepare").prepare().await.unwrap();
    warm(&pool, WIDE_POOL).await;
    let next = AtomicUsize::new(0);
    run_pending(|| {
        let n = next.fetch_add(1, Ordering::SeqCst);
        (
            store(&pool, &format!("tenant-a-{n}")),
            store(&pool, &format!("tenant-b-{n}")),
        )
    });
}

#[tokio::test]
async fn an_entry_survives_a_restart_and_is_taken_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    {
        let first = store(&pool(&db, 2, "sso_pending_one"), "t");
        first.prepare().await.unwrap();
        first.put("binding", &sample_pending("a")).await.unwrap();
    }
    let second = store(&pool(&db, 2, "sso_pending_two"), "t");
    assert_eq!(
        second.take("binding", 10).await.unwrap(),
        Some(sample_pending("a"))
    );
    assert_eq!(second.take("binding", 10).await.unwrap(), None);
}

/// Dump the table and grep: no raw state, nonce, verifier or binding is in any column.
#[tokio::test]
async fn a_stored_row_never_contains_the_raw_secrets() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_pending_dump");
    let store = store(&pool, "dump-tenant");
    store.prepare().await.unwrap();
    store
        .put("raw-browser-binding-value", &sample_pending("zz"))
        .await
        .unwrap();
    let client = db.raw_client().await;
    let rows = client
        .query(
            "SELECT tenant, encode(binding, 'escape'), provider, encode(sealed, 'escape'), \
             encode(binding, 'hex'), encode(sealed, 'hex'), expires_at::text FROM spot_sso_pending",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let mut dump = String::new();
    for row in &rows {
        for column in 0..7 {
            dump.push_str(&row.get::<_, String>(column));
            dump.push('\n');
        }
    }
    for secret in [
        "raw-browser-binding-value",
        "state-zz",
        "nonce-zz",
        "verifier-zz",
        "challenge-zz",
        "ledger.example.com",
    ] {
        assert!(!dump.contains(secret), "the table holds {secret}");
    }
}

#[tokio::test]
async fn a_row_sealed_under_another_key_does_not_open_and_refuses() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_pending_key");
    let writer = PgPendingStore::new(pool.clone(), "t", &KEY, sealer(3)).unwrap();
    writer.prepare().await.unwrap();
    writer.put("b", &sample_pending("a")).await.unwrap();
    let reader = PgPendingStore::new(pool.clone(), "t", &KEY, sealer(4)).unwrap();
    assert_eq!(
        reader.take("b", 10).await.map_err(|e| e.code()),
        Err("sso.pending_store_unavailable")
    );
}

#[tokio::test]
async fn prune_is_bounded_and_spares_live_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_pending_prune");
    let store = store(&pool, "t");
    store.prepare().await.unwrap();
    for n in 0..5 {
        let mut pending = sample_pending("old");
        pending.expires_at = 100;
        store.put(&format!("old-{n}"), &pending).await.unwrap();
    }
    store.put("live", &sample_pending("live")).await.unwrap();
    assert_eq!(store.prune(100, 2).await.unwrap(), 2);
    assert_eq!(store.prune(100, 10).await.unwrap(), 3);
    assert_eq!(store.prune(100, 10).await.unwrap(), 0);
    assert_eq!(
        store.take("live", 10).await.unwrap(),
        Some(sample_pending("live"))
    );
}

#[tokio::test]
async fn preparing_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_pending_prepare");
    let store = store(&pool, "t");
    store.prepare().await.unwrap();
    store.put("b", &sample_pending("a")).await.unwrap();
    store.prepare().await.unwrap();
    assert!(store.take("b", 10).await.unwrap().is_some());
}

/// A store that cannot reach its database refuses; it never answers "none" or "ok".
#[tokio::test]
async fn an_unreachable_database_refuses_every_call() {
    let store = PgPendingStore::new(dead_pool(), "t", &KEY, sealer(3)).unwrap();
    let put = store.put("b", &sample_pending("a")).await;
    assert_eq!(
        put.map_err(|e| e.code()),
        Err("sso.pending_store_unavailable")
    );
    let take = store.take("b", 10).await;
    assert_eq!(
        take.map_err(|e| e.code()),
        Err("sso.pending_store_unavailable")
    );
    let refusal: SsoError = store.prune(0, 1).await.unwrap_err().into();
    assert_eq!(refusal.code(), "sso.pending_store_unavailable");
}

#[test]
fn a_bad_tenant_or_key_builds_no_store() {
    let pool = dead_pool();
    assert!(PgPendingStore::new(pool.clone(), "", &KEY, sealer(3)).is_none());
    assert!(PgPendingStore::new(pool.clone(), "x".repeat(129), &KEY, sealer(3)).is_none());
    assert!(PgPendingStore::new(pool, "t", &[1_u8; 16], sealer(3)).is_none());
}
