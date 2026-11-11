//! The `IdempotencyStore` contract suite on a real database, restart persistence, crash recovery,
//! two-node concurrency, the wait budget, and a database that stops answering.
//!
//! Skips cleanly (a `SKIPPED:` line from `TestDb::create`) when `SPOT_TEST_PG_URL` is unset; with
//! `SPOT_REQUIRE_PG=1` a missing server fails.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{clock, config, fp, pool, response, scope, store, store_on};
use spot_http::api::idempotency_contract::run_all;
use spot_http::api::{IdempotencyError, IdempotencyStore, Reservation};
use spot_idempotency_pg::{ConfigError, PgIdempotencyConfig, PgIdempotencyStore, RetentionPolicy};
use spot_postgres::testing::TestDb;
use tokio::task::JoinSet;

/// The store must pass every case of the backend-independent suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let pool = pool(&db, 40, "idem_contract");
    store_on(pool.clone(), &clock, config()).await;
    let make_clock = clock.clone();
    run_all(
        move || {
            Arc::new(PgIdempotencyStore::new(pool.clone(), make_clock.clone(), config()).unwrap())
        },
        &*clock,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_response_survives_a_restart_byte_for_byte() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let scope = scope("cred", "restart-1");
    let stored = response(201, b"\x00\xff{\"data\":{\"id\":\"1\"}}\r\n");
    {
        // The first process reserves and completes, then goes away (its pool is dropped).
        let first = store(&db, 4, "idem_restart_one", &clock).await;
        assert_eq!(
            first.reserve(&scope, &fp("a")).await,
            Ok(Reservation::Fresh)
        );
        first
            .complete(&scope, &fp("a"), stored.clone(), "http_api")
            .await
            .unwrap();
    }
    // A new process over the same database.
    let second = store(&db, 4, "idem_restart_two", &clock).await;
    assert_eq!(
        second.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Completed(stored)),
        "the retry after a restart gets the identical response"
    );
    assert_eq!(
        second.reserve(&scope, &fp("b")).await,
        Ok(Reservation::Conflict),
        "and the key still belongs to the first request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_reservation_is_pending_until_its_lease_ends_then_free() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let scope = scope("cred", "crash-1");
    {
        let first = store(&db, 2, "idem_crash_one", &clock).await;
        assert_eq!(
            first.reserve(&scope, &fp("a")).await,
            Ok(Reservation::Fresh)
        );
        // The node dies here: no complete, no fail.
    }
    let second = store(&db, 2, "idem_crash_two", &clock).await;
    clock.advance(Duration::from_secs(119));
    assert_eq!(
        second.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Pending),
        "inside the lease the reservation holds"
    );
    assert_eq!(
        second.reserve(&scope, &fp("b")).await,
        Ok(Reservation::Conflict),
        "and a different request is refused, not run"
    );
    clock.advance(Duration::from_secs(2));
    assert_eq!(
        second.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Fresh),
        "after the lease the retry takes the key over (not after 24 h)"
    );
    assert_eq!(
        second.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Pending),
        "and the takeover holds its own lease"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_claim_extends_neither_a_lease_nor_a_window() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store(&db, 2, "idem_extend", &clock).await;
    let pending = scope("cred", "extend-pending");
    assert_eq!(s.reserve(&pending, &fp("a")).await, Ok(Reservation::Fresh));
    clock.advance(Duration::from_secs(100));
    for _ in 0..3 {
        assert_eq!(
            s.reserve(&pending, &fp("a")).await,
            Ok(Reservation::Pending)
        );
    }
    clock.advance(Duration::from_secs(21));
    assert_eq!(
        s.reserve(&pending, &fp("a")).await,
        Ok(Reservation::Fresh),
        "the repeated claims did not push the lease out"
    );

    let done = scope("cred", "extend-done");
    assert_eq!(s.reserve(&done, &fp("a")).await, Ok(Reservation::Fresh));
    s.complete(&done, &fp("a"), response(200, b"{}"), "http_api")
        .await
        .unwrap();
    clock.advance(Duration::from_secs(23 * 3600));
    for _ in 0..3 {
        assert!(matches!(
            s.reserve(&done, &fp("a")).await,
            Ok(Reservation::Completed(_))
        ));
    }
    clock.advance(Duration::from_secs(3601));
    assert_eq!(
        s.reserve(&done, &fp("a")).await,
        Ok(Reservation::Fresh),
        "replays did not push the 24 h window out"
    );
}

/// Sixteen tasks over two pools and two store instances (two nodes) reserve one key at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_racing_for_one_key_see_exactly_one_fresh() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let node_a: Arc<dyn IdempotencyStore> = Arc::new(store(&db, 20, "idem_node_a", &clock).await);
    let node_b: Arc<dyn IdempotencyStore> = Arc::new(store(&db, 20, "idem_node_b", &clock).await);
    let scope = scope("cred", "race-16");
    let fresh = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::new();
    for n in 0..16 {
        let node = if n % 2 == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        let scope = scope.clone();
        let fresh = fresh.clone();
        tasks.spawn(async move {
            loop {
                match node.reserve(&scope, &fp("same")).await.unwrap() {
                    Reservation::Fresh => {
                        fresh.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        node.complete(&scope, &fp("same"), response(201, b"one"), "http_api")
                            .await
                            .unwrap();
                        return None;
                    }
                    Reservation::Pending => node.wait_pending(&scope).await.unwrap(),
                    Reservation::Completed(stored) => return Some(stored),
                    Reservation::Conflict => panic!("same fingerprint conflicted"),
                }
            }
        });
    }
    let mut replays = 0;
    while let Some(done) = tasks.join_next().await {
        if let Some(stored) = done.unwrap() {
            assert_eq!(stored, response(201, b"one"));
            replays += 1;
        }
    }
    assert_eq!(fresh.load(Ordering::SeqCst), 1, "exactly one Fresh");
    assert_eq!(replays, 15, "every other request got the same bytes");
}

/// Fifty tasks over two nodes, half with a different fingerprint: one Fresh, the rest Conflict or
/// Completed, never a second run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_requests_with_two_fingerprints_run_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let node_a: Arc<dyn IdempotencyStore> = Arc::new(store(&db, 30, "idem_mix_a", &clock).await);
    let node_b: Arc<dyn IdempotencyStore> = Arc::new(store(&db, 30, "idem_mix_b", &clock).await);
    let scope = scope("cred", "race-50");
    let fresh = Arc::new(AtomicUsize::new(0));
    let conflicts = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::new();
    for n in 0..50 {
        let node = if n % 2 == 0 {
            node_a.clone()
        } else {
            node_b.clone()
        };
        let scope = scope.clone();
        let (fresh, conflicts) = (fresh.clone(), conflicts.clone());
        let body = if n % 4 < 2 { "left" } else { "right" };
        tasks.spawn(async move {
            let fingerprint = fp(body);
            loop {
                match node.reserve(&scope, &fingerprint).await.unwrap() {
                    Reservation::Fresh => {
                        fresh.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        node.complete(
                            &scope,
                            &fingerprint,
                            response(201, body.as_bytes()),
                            "http_api",
                        )
                        .await
                        .unwrap();
                        return;
                    }
                    Reservation::Pending => node.wait_pending(&scope).await.unwrap(),
                    Reservation::Completed(_) => return,
                    Reservation::Conflict => {
                        conflicts.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap();
    }
    assert_eq!(fresh.load(Ordering::SeqCst), 1, "exactly one Fresh");
    assert!(
        conflicts.load(Ordering::SeqCst) >= 1,
        "the other fingerprint was refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_that_outlasts_its_budget_is_unavailable_and_never_a_second_run() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let config = config().with_wait_budget(Duration::from_millis(700));
    let s = store_on(pool(&db, 4, "idem_budget"), &clock, config).await;
    let scope = scope("cred", "budget-1");
    assert_eq!(s.reserve(&scope, &fp("a")).await, Ok(Reservation::Fresh));
    let started = std::time::Instant::now();
    assert_eq!(
        s.wait_pending(&scope).await,
        Err(IdempotencyError::Unavailable {
            retry_after_secs: 1
        })
    );
    assert!(started.elapsed() >= Duration::from_millis(650));
    assert!(started.elapsed() < Duration::from_secs(5), "bounded");
    assert_eq!(
        s.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Pending),
        "the duplicate is still not Fresh: the original may yet finish"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiter_returns_as_soon_as_the_row_resolves() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = Arc::new(store(&db, 4, "idem_wake", &clock).await);
    let scope = scope("cred", "wake-1");
    assert_eq!(s.reserve(&scope, &fp("a")).await, Ok(Reservation::Fresh));
    let waiter = {
        let (s, scope) = (s.clone(), scope.clone());
        tokio::spawn(async move { s.wait_pending(&scope).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    s.complete(&scope, &fp("a"), response(200, b"{}"), "http_api")
        .await
        .unwrap();
    let woke = tokio::time::timeout(Duration::from_secs(3), waiter).await;
    assert!(matches!(woke, Ok(Ok(Ok(())))), "the poll saw the response");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_complete_wins_when_a_stalled_handler_finishes_after_a_takeover() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store(&db, 2, "idem_stalled", &clock).await;
    let scope = scope("cred", "stalled-1");
    assert_eq!(s.reserve(&scope, &fp("a")).await, Ok(Reservation::Fresh));
    clock.advance(Duration::from_secs(121));
    assert_eq!(
        s.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Fresh),
        "the takeover"
    );
    s.complete(&scope, &fp("a"), response(201, b"first"), "http_api")
        .await
        .unwrap();
    s.complete(&scope, &fp("a"), response(202, b"second"), "http_api")
        .await
        .unwrap();
    assert_eq!(
        s.reserve(&scope, &fp("a")).await,
        Ok(Reservation::Completed(response(201, b"first")))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_whose_pool_is_closed_fails_closed_everywhere() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let p = pool(&db, 2, "idem_closed");
    let s = store_on(p.clone(), &clock, config()).await;
    let scope = scope("cred", "closed-1");
    assert_eq!(s.reserve(&scope, &fp("a")).await, Ok(Reservation::Fresh));
    p.close();
    let down = IdempotencyError::Unavailable {
        retry_after_secs: 1,
    };
    assert_eq!(s.reserve(&scope, &fp("a")).await, Err(down));
    assert_eq!(
        s.reserve(&common::scope("c", "other"), &fp("a")).await,
        Err(down)
    );
    assert_eq!(s.wait_pending(&scope).await, Err(down));
    assert_eq!(
        s.complete(&scope, &fp("a"), response(200, b"{}"), "http_api")
            .await,
        Err(down)
    );
    assert_eq!(s.prune(10).await, Err(IdempotencyError::unavailable()));
    // A release never errors, even with no database.
    s.fail(&scope, &fp("a")).await;
    let text = format!("{down:?} {down}");
    assert!(!text.contains("closed-1") && !text.contains("postgres"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_that_stops_answering_is_unavailable_not_a_held_request() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let config = config().with_operation_timeout(Duration::from_millis(400));
    let s = store_on(pool(&db, 4, "idem_hung"), &clock, config).await;
    // Another session holds the table: every statement of the store waits for the lock.
    let mut holder = db.raw_client().await;
    let tx = holder.transaction().await.unwrap();
    tx.batch_execute("LOCK TABLE spot_idempotency_key IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let answer = s.reserve(&scope("cred", "hung-1"), &fp("a")).await;
    assert_eq!(answer, Err(IdempotencyError::unavailable()));
    assert!(started.elapsed() < Duration::from_secs(3), "bounded");
    drop(tx);
    // With the lock gone the key was never reserved: a retry is Fresh.
    let s = store_on(pool(&db, 4, "idem_hung_after"), &clock, common::config()).await;
    assert_eq!(
        s.reserve(&scope("cred", "hung-1"), &fp("a")).await,
        Ok(Reservation::Fresh)
    );
}

#[tokio::test]
async fn the_constructor_refuses_what_it_cannot_run_safely() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let build = |config: PgIdempotencyConfig| {
        PgIdempotencyStore::new(pool(&db, 1, "idem_config"), clock(), config).err()
    };
    let standard = RetentionPolicy::standard();
    assert_eq!(
        build(PgIdempotencyConfig::new(
            Duration::from_secs(29),
            standard.clone()
        )),
        Some(ConfigError::LeaseTooShort)
    );
    assert_eq!(
        build(PgIdempotencyConfig::new(
            Duration::from_secs(30),
            standard.clone()
        )),
        None,
        "30 s is the floor"
    );
    assert_eq!(
        build(config().with_wait_budget(Duration::ZERO)),
        Some(ConfigError::ZeroWaitBudget)
    );
    assert_eq!(
        build(config().with_max_body_bytes(0)),
        Some(ConfigError::BodyCapOutOfRange)
    );
    assert_eq!(
        build(config().with_max_body_bytes(1_048_577)),
        Some(ConfigError::BodyCapOutOfRange)
    );
    assert_eq!(
        build(config().with_operation_timeout(Duration::ZERO)),
        Some(ConfigError::ZeroOperationTimeout)
    );
}
