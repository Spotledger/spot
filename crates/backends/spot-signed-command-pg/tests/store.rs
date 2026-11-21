//! The PostgreSQL nonce store on a real database: the shared contract suite, restart durability,
//! the two-pool race, fail-closed behaviour, pruning and the schema.
//!
//! Every database test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`; with
//! `SPOT_REQUIRE_PG=1` a missing server fails.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_signed_command::testing::{
    SUITE_SKEW, assert_one_winner, claim_args, failing_store_refuses, fresh_then_replayed,
    reintroduced_after_skew, suite_key,
};
use spot_signed_command::{
    Command, CommandError, CommandVerifier, NonceStore, Signer, SoftwareSigner, StoreError,
    TrustedKeys, VerifyPolicy, sign,
};
use spot_signed_command_pg::{MIGRATION_FAILED, PgNonceStore};
use spot_storage_contract::{Catalog, Collation};
use tokio_postgres::NoTls;

fn pool_of(db: &TestDb, size: usize) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

async fn store(db: &TestDb, size: usize) -> PgNonceStore {
    let store = PgNonceStore::with_skew(pool_of(db, size), SUITE_SKEW);
    store.prepare().await.unwrap();
    store
}

/// A pool whose server refuses connections.
fn unreachable_pool() -> Pool {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".to_owned());
    cfg.port = Some(1);
    cfg.dbname = Some("none".to_owned());
    cfg.user = Some("none".to_owned());
    cfg.password = Some("none".to_owned());
    cfg.pool = Some(PoolConfig::new(2));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

#[test]
fn the_setup_code_is_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    assert!(catalogue.lookup(MIGRATION_FAILED).is_some());
    assert!(MIGRATION_FAILED.starts_with("signed_command_pg."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pg_store_passes_the_contract_suite() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Each check runs against its own keys, so one table serves them all.
    let store = store(&db, 4).await;
    fresh_then_replayed(&store).await;
    reintroduced_after_skew(&store).await;
    failing_store_refuses().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tables_pass_the_migrator_and_prepare_is_repeatable() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2).await;
    store.prepare().await.unwrap();
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    let planned = plan(&live, &wanted);
    assert_eq!(planned.refused.len(), 0, "{:?}", planned.refused);
    let tables = db
        .raw_client()
        .await
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    let tables: Vec<String> = tables.iter().map(|r| r.get(0)).collect();
    assert_eq!(tables, vec!["spot_signed_command_nonce"]);
}

/// A nonce accepted before a restart is still refused after it: the first pool and store are
/// dropped, a new pool reconnects, and the pair is a replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nonce_accepted_before_a_restart_is_refused_after() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let key = suite_key(11);
    {
        let before = store(&db, 2).await;
        assert_eq!(
            before.check_and_insert(&key, [4; 16], 2_000, 1_500).await,
            Ok(true)
        );
    }
    let after = PgNonceStore::with_skew(pool_of(&db, 2), SUITE_SKEW);
    assert_eq!(
        after.check_and_insert(&key, [4; 16], 2_000, 1_500).await,
        Ok(false)
    );
}

/// Two nodes (two pools) race eight claims of one pair: exactly one wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_concurrent_claims_across_two_pools_have_one_winner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let a = Arc::new(store(&db, 6).await);
    let b = Arc::new(PgNonceStore::with_skew(pool_of(&db, 6), SUITE_SKEW));
    let (key, nonce, expires_at, now) = claim_args();
    let mut handles = Vec::new();
    for i in 0..8 {
        let node = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            node.check_and_insert(&key, nonce, expires_at, now).await
        }));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    assert_one_winner(&results);
}

/// Racing claims after the window ran out also give one winner (the update branch).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reclaims_of_an_expired_pair_have_one_winner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(store(&db, 8).await);
    let key = suite_key(12);
    assert_eq!(
        store.check_and_insert(&key, [8; 16], 1_000, 900).await,
        Ok(true)
    );
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            store.check_and_insert(&key, [8; 16], 9_000, 5_000).await
        }));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    assert_one_winner(&results);
}

/// A pool that cannot connect returns the store error, never `true`, with no driver text.
#[tokio::test]
async fn an_unreachable_database_fails_closed() {
    let store = PgNonceStore::new(unreachable_pool());
    let key = suite_key(1);
    let error = store
        .check_and_insert(&key, [1; 16], 2_000, 1_500)
        .await
        .unwrap_err();
    assert_eq!(error, StoreError::Unavailable);
    assert_eq!(error.to_string(), "the nonce store is unavailable");
    assert_eq!(store.prune(1_500).await, Err(StoreError::Unavailable));
    assert!(store.prepare().await.is_err());

    // Through the verifier: a valid command is refused with `cmd.nonce_store`.
    let signer = SoftwareSigner::from_seed(&[5u8; 32]).unwrap();
    let mut trusted = TrustedKeys::new();
    trusted
        .insert(signer.key_id(), &signer.public_key_bytes())
        .unwrap();
    let command = Command::new(
        "server.service.restart",
        serde_json::json!({"service": "x"}),
        "agent-1",
        signer.key_id(),
        2_000,
        2_500,
    );
    let signed = sign(&command, &signer).unwrap();
    let verifier = CommandVerifier::new(trusted, VerifyPolicy::default_policy(), Box::new(store));
    match verifier.verify(&signed, "agent-1", 2_100).await {
        Err(error @ CommandError::Store(_)) => assert_eq!(error.code(), "cmd.nonce_store"),
        Err(other) => panic!("wrong refusal {}", other.code()),
        Ok(_) => panic!("an unreachable store accepted a command"),
    }
}

/// A claim whose table is gone (a failing statement, not a connection) also fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_statement_fails_closed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // No `prepare`: the table does not exist.
    let store = PgNonceStore::new(pool_of(&db, 2));
    let key = suite_key(1);
    assert_eq!(
        store.check_and_insert(&key, [1; 16], 2_000, 1_500).await,
        Err(StoreError::Unavailable)
    );
}

/// `prune` deletes expired rows only, in batches, and a live row stays refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_deletes_only_expired_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4).await;
    let key = suite_key(13);
    for i in 0..5u8 {
        assert_eq!(
            store.check_and_insert(&key, [i; 16], 1_000, 900).await,
            Ok(true)
        );
    }
    assert_eq!(
        store.check_and_insert(&key, [99; 16], 9_000, 900).await,
        Ok(true)
    );
    // At the edge of the skew window nothing is expired.
    assert_eq!(store.prune(1_000 + SUITE_SKEW).await, Ok(0));
    assert_eq!(store.prune(1_000 + SUITE_SKEW + 1).await, Ok(5));
    assert_eq!(store.prune(1_000 + SUITE_SKEW + 1).await, Ok(0));
    // The live row survived and is still a replay.
    assert_eq!(
        store.check_and_insert(&key, [99; 16], 9_000, 1_200).await,
        Ok(false)
    );
}

/// The store's `Debug` shows no connection detail, key or nonce.
#[tokio::test]
async fn debug_shows_configuration_only() {
    let text = format!("{:?}", PgNonceStore::new(unreachable_pool()));
    assert_eq!(text, "PgNonceStore { skew: 60 }");
}
