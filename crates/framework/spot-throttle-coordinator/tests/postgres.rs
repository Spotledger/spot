//! The same cases against the PostgreSQL coordinator, two nodes on one database, plus the two
//! cases only a real server can show: two separate connections racing one account, and a
//! restart over the same database. Every test goes through [`pg_env`], which fails closed when
//! `SPOT_REQUIRE_PG=1` and no server is configured.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod pg_env;
mod suite;

use std::sync::Arc;

use spot_coordinator::Coordinator;
use spot_coordinator_pg::{PgCoordinator, PgCoordinatorConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_throttle_coordinator::Attempt;
use suite::Env;

async fn env() -> Option<Env> {
    let db = pg_env::test_db().await?;
    let mut nodes: Vec<Arc<dyn Coordinator>> = Vec::new();
    for i in 0..2 {
        let coordinator = PgCoordinator::new(
            // A backend (so a pool) per node, as two processes would have; built through the
            // storage crate so this test names no database driver (G5).
            PgBackend::connect_unchecked(db.config()).unwrap().pool(),
            PgCoordinatorConfig::default(),
        );
        if i == 0 {
            coordinator.prepare().await.unwrap();
        }
        nodes.push(Arc::new(coordinator));
    }
    Some(Env {
        nodes,
        clock: None,
        db: Some(db),
    })
}

suite::cases!(env);

// ---------------------------------------------------------------- one account, two connections

/// A backend whose pool holds exactly one connection: this node's own connection to the test
/// database, never a pool another node also draws from.
fn one_connection(db: &TestDb) -> PgBackend {
    let mut config = db.config();
    config.pool_max_size = 1;
    PgBackend::connect_unchecked(config).expect("a pool on the scratch database")
}

/// A sign-in under this test's own tenant, so its buckets are keyed apart from any other test's
/// on the same server.
fn sign_in<'a>(tenant: &'a str, account: &'a str, source: &'a str) -> Attempt<'a> {
    Attempt {
        tenant,
        ..suite::sign_in(account, source)
    }
}

/// Two `CoordinatorThrottle`s over two `PgCoordinator`s, each on its own connection to the same
/// database: 200 concurrent admits for **one** account, split between them, admit exactly the
/// account's limit — never one more — five times over with a fresh account and source each
/// round. This is the shared-store claim tested where it actually has to hold: two connections
/// racing one row, not two handles on one pool in one process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_connections_never_admit_more_than_the_limit() {
    const ROUNDS: usize = 5;
    const CALLS: usize = 200;
    const ACCOUNT_LIMIT: u64 = 37;

    let Some(db) = pg_env::test_db().await else {
        return;
    };
    let prefix = pg_env::key_prefix(&db);

    let mut connections = Vec::new();
    let mut coordinators = Vec::new();
    for node in 0..2 {
        let backend = one_connection(&db);
        assert_eq!(
            backend.pool().status().max_size,
            1,
            "node {node} must have a connection of its own, not a shared pool"
        );
        coordinators.push(PgCoordinator::new(
            backend.pool(),
            PgCoordinatorConfig::default(),
        ));
        // The backends stay alive: dropping one would close the pool its coordinator uses.
        connections.push(backend);
    }
    coordinators[0]
        .prepare()
        .await
        .expect("the coordinator tables");
    let nodes: Vec<Arc<dyn Coordinator>> = coordinators
        .into_iter()
        .map(|coordinator| Arc::new(coordinator) as Arc<dyn Coordinator>)
        .collect();

    // Wide enough that only the account bucket can deny: source, destination and tenant all
    // have room for every call in every round.
    let policy = suite::policy(ACCOUNT_LIMIT, 10_000, 10_000, 100_000);
    for round in 0..ROUNDS {
        let throttles = [
            Arc::new(suite::throttle(&nodes[0], policy)),
            Arc::new(suite::throttle(&nodes[1], policy)),
        ];
        let tenant = format!("{prefix}-t{round}");
        let account = format!("{prefix}-contended-{round}");
        let source = format!("{prefix}-source-{round}");
        let mut calls = Vec::with_capacity(CALLS);
        for index in 0..CALLS {
            let throttle = Arc::clone(&throttles[index % 2]);
            let account = account.clone();
            let source = source.clone();
            let tenant = tenant.clone();
            calls.push(tokio::spawn(async move {
                let attempt = sign_in(&tenant, &account, &source);
                throttle.admit(&attempt).await
            }));
        }
        let mut allowed = 0usize;
        for call in calls {
            let verdict = call
                .await
                .expect("a call task")
                .unwrap_or_else(|error| panic!("round {round}: an admit failed: {error}"));
            if verdict.is_allowed() {
                allowed += 1;
            }
        }
        assert_eq!(
            allowed,
            usize::try_from(ACCOUNT_LIMIT).unwrap(),
            "round {round}: two connections on one account and one limit of {ACCOUNT_LIMIT} \
             admitted {allowed} of {CALLS} calls"
        );
        // Spent on both connections, not just the one that answered last.
        let attempt = sign_in(&tenant, &account, &source);
        for (node, throttle) in throttles.iter().enumerate() {
            assert!(
                !suite::allowed(suite::admit(throttle, &attempt).await),
                "round {round}: node {node} still admits the account both connections spent"
            );
        }
    }
}

/// A bucket lives in the database, not in the process: exhaust it, drop the coordinator and its
/// connection, build a new one over the same database — it is still exhausted, and an account
/// the old coordinator never saw is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exhausted_bucket_survives_a_restart() {
    let Some(db) = pg_env::test_db().await else {
        return;
    };
    let prefix = pg_env::key_prefix(&db);
    let policy = suite::policy(2, 100, 100, 1000);
    let tenant = format!("{prefix}-restart");
    let account = format!("{prefix}-restarted");
    let source = format!("{prefix}-restart-source");

    {
        let backend = one_connection(&db);
        let coordinator = PgCoordinator::new(backend.pool(), PgCoordinatorConfig::default());
        coordinator.prepare().await.expect("the coordinator tables");
        let node: Arc<dyn Coordinator> = Arc::new(coordinator);
        let throttle = suite::throttle(&node, policy);
        for _ in 0..2 {
            assert!(suite::allowed(
                suite::admit(&throttle, &sign_in(&tenant, &account, &source)).await
            ));
        }
        assert!(
            !suite::allowed(suite::admit(&throttle, &sign_in(&tenant, &account, &source)).await),
            "the account's bucket is spent (limit 2)"
        );
    }
    // The coordinator, its pool and its connection are gone here: what is left is the database.

    let backend = one_connection(&db);
    let coordinator = PgCoordinator::new(backend.pool(), PgCoordinatorConfig::default());
    coordinator
        .prepare()
        .await
        .expect("prepare is idempotent, so a restarted node may call it again");
    let node: Arc<dyn Coordinator> = Arc::new(coordinator);
    let throttle = suite::throttle(&node, policy);
    assert!(
        !suite::allowed(suite::admit(&throttle, &sign_in(&tenant, &account, &source)).await),
        "the bucket must still be exhausted after the restart"
    );
    assert!(
        suite::allowed(
            suite::admit(
                &throttle,
                &sign_in(&tenant, &format!("{prefix}-after-restart"), &source)
            )
            .await
        ),
        "the restart must not deny an account the old coordinator never saw"
    );
}
