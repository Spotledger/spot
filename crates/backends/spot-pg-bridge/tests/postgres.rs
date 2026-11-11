//! The bridge against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a message when unset,
//! a failure when `SPOT_REQUIRE_PG=1`).
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use spot_pg_bridge::{BoxFuture, Bridge, BridgeConfig, BridgeError, PooledConn};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};

/// A runtime used only to prepare the scratch database and to inspect it from outside.
fn setup_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    db: TestDb,
}

fn fixture() -> Option<Fixture> {
    let rt = setup_runtime();
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

fn pool_of(config: PgConfig, size: usize) -> deadpool_postgres::Pool {
    let mut config = config;
    config.pool_max_size = size;
    PgBackend::connect_unchecked(config)
        .expect("backend")
        .pool()
}

fn bridge(fx: &Fixture, name: &str, pool: usize, in_flight: usize, queue: usize) -> Bridge {
    let config = BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight: in_flight,
        queue_capacity: queue,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    };
    Bridge::new(config, pool_of(fx.db.config(), pool)).expect("bridge")
}

fn sleep_query(
    seconds: f64,
) -> impl FnOnce(PooledConn) -> BoxFuture<'static, Result<i32, tokio_postgres::Error>> + Send + 'static
{
    move |conn| {
        Box::pin(async move {
            conn.query_one("SELECT 1 FROM pg_sleep($1)", &[&seconds])
                .await
                .map(|_| 1)
        })
    }
}

#[test]
fn a_query_runs_and_a_store_error_comes_back_as_the_stores_own() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, "pg_basic", 6, 3, 8);
    let one = bridge
        .run_default(|conn| {
            Box::pin(async move {
                conn.query_one("SELECT 41 + 1", &[])
                    .await
                    .map(|r| r.get::<_, i32>(0))
            })
        })
        .expect("query");
    assert_eq!(one, 42);
    let failed: Result<i32, _> = bridge.run_default(|conn| {
        Box::pin(async move {
            conn.query_one("SELECT * FROM table_that_does_not_exist", &[])
                .await
                .map(|_| 0)
        })
    });
    match failed {
        Err(BridgeError::Store(e)) => assert!(e.as_db_error().is_some()),
        other => panic!("expected the driver's error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_inside_a_multi_thread_test_runtime_is_wrong_thread() {
    let Some(fx) = tokio::task::spawn_blocking(fixture).await.expect("join") else {
        return;
    };
    let bridge = bridge(&fx, "pg_wt_mt", 6, 3, 8);
    assert!(matches!(
        bridge.run_default(sleep_query(0.0)),
        Err(BridgeError::WrongThread)
    ));
    // The fixture owns a runtime, which must be dropped off this runtime's threads.
    tokio::task::spawn_blocking(move || drop(fx))
        .await
        .expect("join");
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_inside_a_current_thread_test_runtime_is_wrong_thread() {
    let Some(fx) = tokio::task::spawn_blocking(fixture).await.expect("join") else {
        return;
    };
    let bridge = bridge(&fx, "pg_wt_ct", 6, 3, 8);
    assert!(matches!(
        bridge.run_default(sleep_query(0.0)),
        Err(BridgeError::WrongThread)
    ));
    // The fixture owns a runtime, which must be dropped off this runtime's threads.
    tokio::task::spawn_blocking(move || drop(fx))
        .await
        .expect("join");
}

#[test]
fn overload_ends_cleanly_and_the_bridge_recovers() {
    let Some(fx) = fixture() else { return };
    let in_flight = 4;
    let bridge = Arc::new(bridge(&fx, "pg_overload", in_flight + 3, in_flight, 16));
    let started = Instant::now();
    let handles: Vec<_> = (0..10 * in_flight)
        .map(|_| {
            let bridge = Arc::clone(&bridge);
            thread::spawn(move || bridge.run(Duration::from_millis(1_500), sleep_query(0.05)))
        })
        .collect();
    let mut ok = 0;
    for handle in handles {
        match handle.join().expect("no caller hangs") {
            Ok(_) => ok += 1,
            Err(BridgeError::Busy | BridgeError::Timeout) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert!(ok >= in_flight, "work was done: {ok}");
    // Ideal is 40 x 50 ms over 4 slots = 0.5 s; the bound is the deadline plus generous slack.
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        bridge.run_default(sleep_query(0.0)).is_ok(),
        "recovers after the load"
    );
}

#[test]
fn the_largest_in_flight_the_constructor_allows_does_not_deadlock_with_a_second_query() {
    let Some(fx) = fixture() else { return };
    let pool = 6;
    let in_flight = BridgeConfig::largest_in_flight(pool);
    assert_eq!(in_flight, 4);
    let bridge = Arc::new(bridge(&fx, "pg_pool_limit", pool, in_flight, 16));
    let handles: Vec<_> = (0..in_flight * 2)
        .map(|_| {
            let bridge = Arc::clone(&bridge);
            thread::spawn(move || {
                bridge.run(
                    Duration::from_secs(10),
                    |conn| -> BoxFuture<'static, Result<i32, tokio_postgres::Error>> {
                        Box::pin(async move {
                            conn.query_one("SELECT 1 FROM pg_sleep(0.1)", &[]).await?;
                            // A second query on the same connection, inside the closure.
                            conn.query_one("SELECT 2", &[])
                                .await
                                .map(|r| r.get::<_, i32>(0))
                        })
                    },
                )
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().expect("join").expect("completes"), 2);
    }
}

#[test]
fn a_timeout_cancels_the_statement_and_the_pool_stays_usable() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, "pg_cancel", 6, 2, 4);
    let started = Instant::now();
    let result = bridge.run(Duration::from_millis(300), sleep_query(30.0));
    assert!(matches!(result, Err(BridgeError::Timeout)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The statement is gone from the server, not left running for 30 s.
    let running = fx.rt.block_on(async {
        let admin = fx.db.admin_client().await;
        for _ in 0..40 {
            let row = admin
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND query LIKE '%pg_sleep($1)%' AND state = 'active' AND pid <> pg_backend_pid()",
                    &[&fx.db.name()],
                )
                .await
                .expect("activity");
            if row.get::<_, i64>(0) == 0 {
                return 0;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        1
    });
    assert_eq!(running, 0, "the cancelled statement is still active");
    assert!(
        bridge.run_default(sleep_query(0.0)).is_ok(),
        "a later call succeeds"
    );
}

#[test]
fn shutdown_with_a_call_in_flight_returns_within_grace() {
    let Some(fx) = fixture() else { return };
    let bridge = Arc::new(bridge(&fx, "pg_shutdown", 6, 2, 4));
    let caller = {
        let bridge = Arc::clone(&bridge);
        thread::spawn(move || bridge.run(Duration::from_secs(20), sleep_query(30.0)))
    };
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    bridge
        .shutdown(Duration::from_millis(500))
        .expect("shutdown");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
    assert!(matches!(
        caller.join().expect("join"),
        Err(BridgeError::Closed)
    ));
    assert!(matches!(
        bridge.run_default(sleep_query(0.0)),
        Err(BridgeError::Closed)
    ));
}

#[test]
fn an_inconsistent_config_against_a_real_pool_is_refused() {
    let Some(fx) = fixture() else { return };
    let config = BridgeConfig {
        max_in_flight: 5,
        ..BridgeConfig::default()
    };
    // Pool of 5: in-flight 5 would use every connection.
    match Bridge::new(config, pool_of(fx.db.config(), 5)) {
        Err(BridgeError::Config { setting, .. }) => assert_eq!(setting, "max_in_flight"),
        other => panic!("expected a config error, got {:?}", other.err()),
    }
}

/// A bridge that runs one call at a time over a fresh pool, so each call takes the one connection
/// the pool has created: the connection is reused, and a setting it carries is observed.
fn sequential_bridge(fx: &Fixture, name: &str) -> Bridge {
    let config = BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight: 1,
        queue_capacity: 4,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    };
    Bridge::new(config, pool_of(fx.db.config(), 3)).expect("bridge")
}

/// The server process of the connection, and the `application_name` the session carries.
fn pid_and_application_name(
    conn: PooledConn,
) -> BoxFuture<'static, Result<(i32, String), tokio_postgres::Error>> {
    Box::pin(async move {
        let row = conn
            .query_one(
                "SELECT pg_backend_pid(), current_setting('application_name')",
                &[],
            )
            .await?;
        Ok((row.get::<_, i32>(0), row.get::<_, String>(1)))
    })
}

/// The documented hazard, pinned: the pool recycles with `RecyclingMethod::Fast` and the bridge
/// resets nothing, so a session-level `SET` made by one call is visible in the next call on that
/// connection — and, since the same statement runs here, `SET LOCAL` inside a transaction is not.
/// A README that promised otherwise would fail this test.
#[test]
fn a_session_set_is_visible_in_the_next_call_and_a_scoped_set_is_not() {
    let Some(fx) = fixture() else { return };
    let bridge = sequential_bridge(&fx, "pg_session_state");

    let (first_pid, before) = bridge
        .run_default(pid_and_application_name)
        .expect("first call");
    assert_ne!(
        before, "bridge_session_state",
        "the pool does not name its sessions that, as this test assumes"
    );

    bridge
        .run_default(|conn| {
            Box::pin(async move {
                conn.batch_execute("SET application_name = 'bridge_session_state'")
                    .await
            })
        })
        .expect("a session-level SET");

    let (second_pid, seen) = bridge
        .run_default(pid_and_application_name)
        .expect("second call");
    assert_eq!(
        second_pid, first_pid,
        "the second call must reuse the one connection, or the hazard is not tested"
    );
    assert_eq!(
        seen, "bridge_session_state",
        "a plain SET must survive the call: the bridge resets nothing and the pool recycles fast"
    );

    // The same setting, scoped to a transaction. It applies inside it, and ends with it.
    let inside: String = bridge
        .run_default(|mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                tx.batch_execute("SET LOCAL application_name = 'bridge_scoped'")
                    .await?;
                let row = tx
                    .query_one("SELECT current_setting('application_name')", &[])
                    .await?;
                let inside: String = row.get(0);
                tx.commit().await?;
                Ok::<_, tokio_postgres::Error>(inside)
            })
        })
        .expect("a scoped SET");
    assert_eq!(
        inside, "bridge_scoped",
        "SET LOCAL must apply inside its transaction"
    );

    let (third_pid, after) = bridge
        .run_default(pid_and_application_name)
        .expect("third call");
    assert_eq!(third_pid, first_pid, "the pool reused the same connection");
    assert_eq!(
        after, "bridge_session_state",
        "SET LOCAL must not leak out of its transaction: the session still carries the plain SET"
    );

    bridge
        .run_default(|conn| {
            Box::pin(async move { conn.batch_execute("SET application_name = DEFAULT").await })
        })
        .expect("reset the session");
}

/// A closure that panics is the bridge's own `closed`: the connection it held is discarded, not
/// pooled, so the next caller gets a fresh session, and the bridge keeps serving.
#[test]
fn a_panicking_closure_discards_its_connection_and_the_bridge_keeps_serving() {
    let Some(fx) = fixture() else { return };
    let bridge = sequential_bridge(&fx, "pg_panic_discard");

    // One connection exists in the pool at this point. Were it returned to the pool after the
    // panic, it would be the only idle one and the last call would take it back: same pid.
    let (first_pid, _) = bridge
        .run_default(pid_and_application_name)
        .expect("first call");

    let failed: Result<(), BridgeError<tokio_postgres::Error>> = bridge.run_default(
        |_conn| -> BoxFuture<'static, Result<(), tokio_postgres::Error>> {
            panic!("a closure defect, with a healthy connection in hand")
        },
    );
    assert!(matches!(failed, Err(BridgeError::Closed)), "{failed:?}");

    let (after_pid, _) = bridge
        .run_default(pid_and_application_name)
        .expect("the bridge keeps serving");
    assert_ne!(
        after_pid, first_pid,
        "the connection of the panicking call came back to the pool instead of being discarded"
    );
}
