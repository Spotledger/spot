//! The `ReplayStore` contract suite, restart persistence, the two-pool race and `prune` on a
//! real database.
//!
//! Skips cleanly (a `SKIPPED:` line from `TestDb::create`) when `SPOT_TEST_PG_URL` is unset; with
//! `SPOT_REQUIRE_PG=1` a missing server fails. The CI Postgres service sets the URL and the suite
//! runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{POOL, namespaced, pool, store, warm};
use spot_postgres::testing::TestDb;
use spot_sso::{Claim, ReplayError, ReplayStore, contract, namespaced as key};
use spot_sso_pg::PgReplayStore;

/// The store must pass every case of the backend-independent suite.
///
/// The suite is synchronous and drives each claim on the calling thread ([`contract::block_on`]),
/// so the racing case claims from sixteen plain threads. Those threads have no reactor, so the
/// pool is warmed to `POOL` connections here — a connection created mid-race would need one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, POOL, "sso_contract");
    // One prepared table serves every `make()`; each store only differs in its namespace.
    PgReplayStore::new(pool.clone(), "contract")
        .prepare()
        .await
        .unwrap();
    warm(&pool, POOL).await;
    // Every `make()` must hand back an empty store: a fresh namespace on the shared table is one.
    let next = AtomicUsize::new(0);
    contract::run_all(|| {
        PgReplayStore::new(
            pool.clone(),
            format!("contract-{}", next.fetch_add(1, Ordering::SeqCst)),
        )
    });
}

#[tokio::test]
async fn a_claim_survives_a_restart() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let id = namespaced("saml-assertion", "okta", "restart-1");
    {
        // The first process claims the assertion, then goes away (its pool is dropped).
        let first = store(&db, 4, "restart", "sso_restart_one").await;
        assert_eq!(first.claim(&id, 100, 200).await.unwrap(), Claim::Fresh);
    }
    // A new process over the same database, pool and namespace.
    let second = store(&db, 4, "restart", "sso_restart_two").await;
    assert_eq!(
        second.claim(&id, 150, 200).await.unwrap(),
        Claim::Replayed,
        "the id the first process claimed is still held"
    );
    assert_eq!(
        second.claim(&id, 200, 300).await.unwrap(),
        Claim::Fresh,
        "and is free again at its expiry, across the restart"
    );
}

#[tokio::test]
async fn a_restart_does_not_extend_an_expiry() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let id = namespaced("oidc-logout", "entra", "restart-2");
    let first = store(&db, 2, "expiry", "sso_expiry_one").await;
    assert_eq!(first.claim(&id, 100, 200).await.unwrap(), Claim::Fresh);
    drop(first);
    let second = store(&db, 2, "expiry", "sso_expiry_two").await;
    // A refused replay of the restored row must not push its expiry out.
    assert_eq!(
        second.claim(&id, 150, 10_000).await.unwrap(),
        Claim::Replayed
    );
    assert_eq!(
        second.claim(&id, 200, 300).await.unwrap(),
        Claim::Fresh,
        "the expiry recorded before the restart still stands"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_racing_claims_across_two_pools_give_exactly_one_fresh() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let id = namespaced("saml-logout", "okta", "racing-1");
    let left = store(&db, 4, "race", "sso_race_left").await;
    let right = store(&db, 4, "race", "sso_race_right").await;
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let store = if i % 2 == 0 {
                left.clone()
            } else {
                right.clone()
            };
            let id = id.clone();
            tokio::spawn(async move { store.claim(&id, 100, 200).await })
        })
        .collect();
    let mut fresh = 0;
    for handle in handles {
        match handle.await.unwrap().unwrap() {
            Claim::Fresh => fresh += 1,
            Claim::Replayed => {}
        }
    }
    assert_eq!(fresh, 1, "exactly one of the eight claims is fresh");
}

#[tokio::test]
async fn namespaces_never_share_an_id() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let id = key("oidc-logout", "okta", "same-id");
    let tenant_a = store(&db, 2, "tenant-a", "sso_ns_a").await;
    let tenant_b = store(&db, 2, "tenant-b", "sso_ns_b").await;
    assert_eq!(tenant_a.claim(&id, 100, 200).await.unwrap(), Claim::Fresh);
    assert_eq!(
        tenant_b.claim(&id, 100, 200).await.unwrap(),
        Claim::Fresh,
        "the same id in another namespace is another row"
    );
    assert_eq!(
        tenant_a.claim(&id, 100, 200).await.unwrap(),
        Claim::Replayed,
        "and each namespace keeps its own claim"
    );
}

#[tokio::test]
async fn prepare_can_run_again_on_a_prepared_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2, "prepare", "sso_prepare").await;
    store.prepare().await.unwrap();
    store.prepare().await.unwrap();
    let id = namespaced("saml-assertion", "okta", "prepared-twice");
    assert_eq!(store.claim(&id, 100, 200).await.unwrap(), Claim::Fresh);
}

#[tokio::test]
async fn prune_deletes_expired_rows_in_bounded_batches() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2, "prune", "sso_prune").await;
    let live = namespaced("saml-assertion", "okta", "live");
    // Five rows that are expired at 100 and one that is not.
    for n in 0..5 {
        let id = namespaced("oidc-logout", "okta", &format!("expired-{n}"));
        assert_eq!(store.claim(&id, 10, 40 + n).await.unwrap(), Claim::Fresh);
    }
    assert_eq!(store.claim(&live, 10, 1_000).await.unwrap(), Claim::Fresh);

    // Each call removes at most its limit.
    assert_eq!(store.prune(100, 2).await.unwrap(), 2);
    assert_eq!(store.prune(100, 2).await.unwrap(), 2);
    assert_eq!(store.prune(100, 2).await.unwrap(), 1);
    assert_eq!(store.prune(100, 2).await.unwrap(), 0, "nothing left");

    // The live row survived every batch and still refuses a replay.
    assert_eq!(
        store.claim(&live, 100, 1_000).await.unwrap(),
        Claim::Replayed
    );
    // A pruned (expired) id is claimable again — pruning loses nothing the contract promises.
    let pruned = namespaced("oidc-logout", "okta", "expired-0");
    assert_eq!(store.claim(&pruned, 100, 200).await.unwrap(), Claim::Fresh);
}

/// A prune that reaches an expired id while a claim is re-recording it must spare that row.
///
/// The re-claim is staged as the store's own conflict arm does it: an `UPDATE` of `expires_at`
/// held open in a transaction, so the row lock is taken and the new expiry is not yet visible.
/// `prune` then selects the row from its older snapshot and waits for that lock. When the claim
/// commits, the delete must not remove the just-reclaimed id — if it did, the message could be
/// replayed a second time. The wait is observed in `pg_stat_activity`, so the case races for real
/// instead of relying on a sleep.
#[tokio::test]
async fn a_prune_in_flight_spares_an_id_claimed_again() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2, "prune-race", "sso_prune_race").await;
    let id = namespaced("saml-assertion", "okta", "reclaimed");
    assert_eq!(store.claim(&id, 10, 40).await.unwrap(), Claim::Fresh);

    let mut claim_conn = db.raw_client().await;
    let claim = claim_conn.transaction().await.unwrap();
    let updated = claim
        .execute(
            "UPDATE spot_sso_replay SET expires_at = $3 WHERE namespace = $1 AND id = $2",
            &[&"prune-race", &id, &500_i64],
        )
        .await
        .unwrap();
    assert_eq!(updated, 1, "the expired row is there to reclaim");

    let observer = db.raw_client().await;
    let pruner = {
        let store = store.clone();
        tokio::spawn(async move { store.prune(100, 10).await })
    };

    // The delete is blocked on the claim's row lock: the race is in flight, not assumed.
    let mut waited = false;
    for _ in 0..250 {
        let waiting: i64 = observer
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE query LIKE 'DELETE FROM spot_sso_replay%' AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            waited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(waited, "the prune reached the row and waited for the claim");

    claim.commit().await.unwrap();
    let deleted = pruner.await.unwrap().unwrap();
    assert_eq!(
        deleted, 0,
        "the row was live again by the time the delete ran"
    );
    assert_eq!(
        store.claim(&id, 100, 900).await.unwrap(),
        Claim::Replayed,
        "the id claimed again mid-prune is still held"
    );
}

#[tokio::test]
async fn the_expires_index_serves_the_prune_selection() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2, "index", "sso_index").await;
    let raw = db.raw_client().await;
    // A sequential scan is always the planner's choice on a tiny table; turning it off asks the
    // question the index exists for: is there an index that can answer this predicate?
    raw.batch_execute("SET enable_seqscan = off").await.unwrap();
    let rows = raw
        .query(
            "EXPLAIN (COSTS OFF) SELECT namespace, id FROM spot_sso_replay \
             WHERE expires_at <= $1 LIMIT $2",
            &[&100_i64, &2_i64],
        )
        .await
        .unwrap();
    let plan = rows
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan.contains("spot_sso_replay_expires_idx"),
        "the prune selection reads the expires_at index; plan was:\n{plan}"
    );
    drop(store);
}

#[tokio::test]
async fn a_closed_pool_fails_closed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_closed");
    let store = PgReplayStore::new(pool.clone(), "closed");
    store.prepare().await.unwrap();
    pool.close();
    let error = store
        .claim(&namespaced("saml-assertion", "okta", "closed"), 0, 10)
        .await
        .unwrap_err();
    assert_eq!(error, ReplayError::unavailable());
    assert_eq!(error.code(), "sso.replay_store_unavailable");
    assert!(
        store.prune(0, 10).await.is_err(),
        "a prune that cannot reach the database is an error too"
    );
}

#[tokio::test]
async fn a_database_failure_fails_closed_without_driver_text() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 2, "broken", "sso_broken").await;
    // A table that is not there: the store cannot vouch for anything, so it refuses.
    db.raw_client()
        .await
        .batch_execute("DROP TABLE spot_sso_replay")
        .await
        .unwrap();
    let error = store
        .claim(&namespaced("saml-assertion", "okta", "broken"), 0, 10)
        .await
        .unwrap_err();
    assert_eq!(error, ReplayError::unavailable());
    let text = error.to_string();
    assert!(
        !text.contains("spot_sso_replay") && !text.contains("ERROR"),
        "no driver or schema text reaches a caller: {text}"
    );
}
