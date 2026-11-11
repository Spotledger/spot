//! [`PgBackend::pool`]: the sibling stores of a site share the backend's one pool.
//!
//! [`PgBackend::cancel_tls`]: the connector that pool uses, handed out for the statement-cancel
//! request of `spot-pg-bridge`. These three tests need no server — `connect_unchecked` opens no
//! connection — and are skipped only when this host has no platform root certificates.
//!
//! Skipped when no server is configured (`SPOT_TEST_PG_URL`), like the crate's other database
//! tests; `SPOT_REQUIRE_PG=1` makes the skip a failure.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{env_with, options};
use spot_postgres::{PgBackend, PgConfig, Tls};
use spot_storage_contract::{ErrorKind, HealthCheck, Isolation, StorageBackend, Transaction};
use tokio_postgres::Socket;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres_rustls::MakeRustlsConnect;

/// Slack allowed on top of a configured wait timeout, as in the other pool tests.
const MARGIN: Duration = Duration::from_millis(1500);

/// A backend whose config requires TLS, built without a server (`connect_unchecked` contacts
/// nothing), or `None` after one `SKIPPED:` line when this host has no platform root certificates.
///
/// Loading those roots is the only thing that can fail here: the config is valid and nothing is
/// connected, so any other error is a real failure and panics.
fn tls_backend() -> Option<PgBackend> {
    let mut config = PgConfig::new("127.0.0.1", 5432, "spot", "spot", "not-a-real-password");
    config.tls = Tls::Require;
    match PgBackend::connect_unchecked(config) {
        Ok(backend) => Some(backend),
        Err(error) => {
            assert!(
                error.explanation.contains("root certificates"),
                "a backend with Tls::Require failed for a reason other than the root store: {error}"
            );
            println!("SKIPPED: no platform root certificates to verify a TLS connection with");
            None
        }
    }
}

/// A pool that is plain hands out no TLS connector: the cancel request of a timed-out statement
/// must then go over `NoTls` too, which is exactly the transport the pool itself uses.
#[test]
fn a_backend_without_tls_has_no_cancel_connector() {
    let config = PgConfig::new("127.0.0.1", 5432, "spot", "spot", "not-a-real-password");
    assert_eq!(
        config.tls,
        Tls::Disable,
        "the default config is plain, as this test assumes"
    );
    let backend = PgBackend::connect_unchecked(config).expect("a plain backend needs no server");

    assert!(
        backend.cancel_tls().is_none(),
        "a plain pool has no connector to hand out: a cancel over NoTls matches the pool"
    );
}

/// A pool that requires TLS hands out the connector it was built with, so the bridge can send the
/// cancel request over TLS instead of in clear.
#[test]
fn a_backend_with_tls_hands_out_its_cancel_connector() {
    let Some(backend) = tls_backend() else {
        return;
    };
    assert!(
        backend.cancel_tls().is_some(),
        "a TLS pool must offer its connector: otherwise the cancel key travels in clear"
    );
}

/// Two calls return clones of the one connector the pool was built with: the same client config
/// and the same root store, copied as a handle — never rebuilt, and never a second trust store
/// that could disagree with the pool's.
#[test]
fn two_calls_return_connectors_that_share_their_config() {
    let Some(backend) = tls_backend() else {
        return;
    };
    let first = backend.cancel_tls().expect("the first connector");
    let second = backend.cancel_tls().expect("the second connector");

    // `MakeRustlsConnect` is one `Arc<ClientConfig>` and nothing else, so a pointer is its whole
    // state: a clone copies that pointer and bumps a reference count over the same roots and
    // verifier, which cannot rebuild either. Should the dependency ever stop being a single
    // shared handle, this fails and the "cheap clone" promise of `cancel_tls` needs re-checking.
    assert_eq!(
        std::mem::size_of::<MakeRustlsConnect>(),
        std::mem::size_of::<usize>(),
        "the connector is expected to be a single shared handle"
    );

    // Each handle still describes that same, complete config: it can start a TLS connection to
    // the database's host, and neither call had to load a certificate to make it.
    for mut connector in [first, second.clone(), second] {
        assert!(
            <MakeRustlsConnect as MakeTlsConnect<Socket>>::make_tls_connect(
                &mut connector,
                "db.example.invalid"
            )
            .is_ok(),
            "a returned connector cannot make a TLS connect"
        );
    }
}

/// The `pool` health check of a backend and its `(in use, idle, at most)` counts.
async fn pool_check(backend: &PgBackend) -> (HealthCheck, usize, usize, usize) {
    let report = backend.health().await;
    let check = report
        .checks
        .into_iter()
        .find(|c| c.name == "pool")
        .expect("a pool health check");
    let words: Vec<&str> = check.detail.split_whitespace().collect();
    assert!(words.len() >= 10, "{}", check.detail);
    let count = |i: usize| words[i].parse::<usize>().expect("a count");
    assert_eq!(words[1], "in", "{}", check.detail);
    assert_eq!(words[2], "use,", "{}", check.detail);
    assert_eq!(words[4], "idle,", "{}", check.detail);
    assert_eq!(words[6], "waiting,", "{}", check.detail);
    assert_eq!(words[7], "at", "{}", check.detail);
    assert_eq!(words[8], "most", "{}", check.detail);
    let (in_use, idle, max) = (count(0), count(3), count(9));
    (check, in_use, idle, max)
}

#[tokio::test]
async fn a_connection_from_the_returned_pool_is_counted_in_the_backends_health_check() {
    let Some(env) = env_with(|c| {
        c.pool_max_size = 2;
        c.pool_wait_timeout = Duration::from_millis(300);
    })
    .await
    else {
        return;
    };
    env.table().await;

    let pool = env.backend.pool();
    let (check, in_use, _idle, max) = pool_check(&env.backend).await;
    assert_eq!(
        max, 2,
        "the handle is bounded like the backend's own pool: {check:?}"
    );
    assert_eq!(in_use, 0, "nothing is out of the pool yet: {check:?}");

    // Both slots, taken from the handle, show up in the backend's own health check.
    let first = pool.get().await.unwrap();
    let second = pool.get().await.unwrap();
    let (check, in_use, idle, _max) = pool_check(&env.backend).await;
    assert_eq!(
        in_use, 2,
        "the backend counts the sibling's connections as its own: {check:?}"
    );
    assert_eq!(idle, 0, "{check:?}");

    // It is the same database too, not just the same pool.
    let row = first
        .query_one("SELECT v FROM t WHERE id = 1", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 10);

    // One pool and one `max_size`: with both slots out to the sibling, the backend waits and
    // gives up rather than opening a connection of its own.
    let started = Instant::now();
    let err = env
        .begin(options("competing", Isolation::ReadCommitted))
        .await
        .err()
        .expect("the backend had no connection left");
    let waited = started.elapsed();
    assert_eq!(err.kind, ErrorKind::Unavailable, "{err}");
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_millis(300) + MARGIN,
        "the backend waited on the shared pool: {waited:?}"
    );

    // Dropping the handle is harmless: the pool is the backend's and stays open.
    drop(pool);
    let (check, in_use, _idle, _max) = pool_check(&env.backend).await;
    assert_eq!(
        in_use, 2,
        "the backend's pool outlives the dropped handle: {check:?}"
    );

    // Giving the connections back leaves the backend working, on that same pool.
    drop((first, second));
    let (check, in_use, idle, _max) = pool_check(&env.backend).await;
    assert_eq!((in_use, idle), (0, 2), "{check:?}");

    let mut tx = env
        .begin(options("after_drop", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (7, 70)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(env.value(7).await, Some(70));
}
