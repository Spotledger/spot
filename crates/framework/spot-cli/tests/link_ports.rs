//! The link-service providers: the hold probe, the resilience factory and the nonce stores, with
//! one conformance test the file store and the PostgreSQL store both pass (the PostgreSQL part is
//! skipped with a printed reason when `SPOT_TEST_PG_URL` is unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_cli::services::link_exchange::{ResilienceFactory, SendHold};
use spot_cli::services::link_external::{FileNonceStore, NonceStore};
use spot_cli::services::link_ports::{PgNonceStore, ProbeHold, SeededResilience};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use tokio::sync::Semaphore;

#[test]
fn an_unknown_hold_state_is_held() {
    assert!(ProbeHold::new(|| None).held());
    assert!(ProbeHold::new(|| Some(true)).held());
    assert!(!ProbeHold::new(|| Some(false)).held());
}

#[test]
fn each_kit_shares_the_bulkhead() {
    let bulkhead = Arc::new(Semaphore::new(4));
    let factory = SeededResilience::new(7);
    let a = factory.make(Arc::clone(&bulkhead));
    let b = factory.make(Arc::clone(&bulkhead));
    assert!(Arc::ptr_eq(&a.bulkhead, &b.bulkhead));
}

/// What every nonce store must do: first claim wins, a replay is refused inside the window, an
/// expired claim can be made again, peers do not share nonces.
fn conformance(store: &dyn NonceStore) {
    assert_eq!(store.claim("acme", "n1", 1_000, 301_000), Ok(true));
    assert_eq!(store.claim("acme", "n1", 2_000, 302_000), Ok(false));
    assert_eq!(store.claim("other", "n1", 2_000, 302_000), Ok(true));
    assert_eq!(store.claim("acme", "n2", 2_000, 302_000), Ok(true));
    assert_eq!(store.claim("acme", "n1", 400_000, 700_000), Ok(true));
}

#[test]
fn the_file_store_conforms() {
    let path = std::env::temp_dir().join(format!("spot-nonce-conf-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = FileNonceStore::open(&path, 100, 0).unwrap();
    conformance(&store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_postgresql_store_conforms_survives_restart_and_holds_across_nodes() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let handle = tokio::runtime::Handle::current();
    let first = PgNonceStore::new(backend.pool(), handle.clone(), "link_ext").unwrap();
    first.prepare().await.unwrap();
    conformance(&first);

    // A restart (a new store over the same database) still refuses a replay inside the window.
    assert_eq!(first.claim("acme", "r1", 10_000, 310_000), Ok(true));
    drop(first);
    let restarted = PgNonceStore::new(backend.pool(), handle.clone(), "link_ext").unwrap();
    assert_eq!(restarted.claim("acme", "r1", 11_000, 311_000), Ok(false));

    // A second node (its own pool) sees the same claims.
    let other_backend = PgBackend::connect(db.config()).await.unwrap();
    let node_two = PgNonceStore::new(other_backend.pool(), handle, "link_ext").unwrap();
    assert_eq!(node_two.claim("acme", "r1", 12_000, 312_000), Ok(false));
    assert_eq!(node_two.claim("acme", "r2", 12_000, 312_000), Ok(true));
    assert_eq!(restarted.claim("acme", "r2", 13_000, 313_000), Ok(false));
}
