//! Three nodes on one database: the job runner that never double-runs, handover, and a rate
//! limit shared by all three.
//!
//! Three `PgCoordinator`s on three pools stand in for three processes; every one coordinates
//! through the same database. A "job runner" ticks across them, taking one lease per job name and
//! writing a fenced "ran" row; the fenced primary key means a job runs at most once, and a stale
//! holder (a lease that expired mid-run) is refused by the fence before it can write. Skips
//! without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{coordinator, db};
use spot_coordinator::{Admit, Coordinator, FencingToken, RateLimit};
use spot_coordinator_pg::{PgCoordinator, PgCoordinatorConfig, assert_fence};

/// A node: a coordinator on its own pool and a raw client for its fenced business transaction.
struct Node {
    coordinator: Arc<PgCoordinator>,
    client: tokio::sync::Mutex<tokio_postgres::Client>,
}

async fn node(db: &spot_postgres::testing::TestDb, config: PgCoordinatorConfig) -> Node {
    Node {
        coordinator: Arc::new(coordinator(db, config, 8).await),
        client: tokio::sync::Mutex::new(db.raw_client().await),
    }
}

#[derive(Default)]
struct Counts {
    conflicts: AtomicU64,
    superseded: AtomicU64,
}

/// Takes the lease for `job`, fences a write, and records whether the runner was superseded
/// before it could write. `slow` makes the holder sleep past the ttl to force a takeover.
async fn tick(node: &Node, job: &str, holder: &str, ttl: Duration, slow: bool, counts: &Counts) {
    let Ok(lease) = node.coordinator.acquire(job, holder, ttl).await else {
        return;
    };
    if slow {
        tokio::time::sleep(ttl + Duration::from_millis(150)).await;
    }
    let mut client = node.client.lock().await;
    let Ok(tx) = client.transaction().await else {
        let _ = node.coordinator.release(&lease).await;
        return;
    };
    match assert_fence(&tx, job, lease.token).await {
        Ok(()) => {
            let done = tx
                .query_opt("SELECT 1 FROM ran WHERE job = $1", &[&job])
                .await
                .expect("check ran")
                .is_some();
            if !done {
                let token = i64::try_from(lease.token.0).unwrap_or(i64::MAX);
                let inserted = tx
                    .execute(
                        "INSERT INTO ran (job, token) VALUES ($1, $2) ON CONFLICT (job) DO NOTHING",
                        &[&job, &token],
                    )
                    .await
                    .expect("insert ran");
                if inserted == 0 {
                    // Two holders both saw the job unrun: the fence or the lease failed.
                    counts.conflicts.fetch_add(1, Ordering::SeqCst);
                }
            }
            tx.commit().await.expect("commit");
        }
        Err(_) => {
            counts.superseded.fetch_add(1, Ordering::SeqCst);
            let _ = tx.rollback().await;
        }
    }
    let _ = node.coordinator.release(&lease).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_thousand_ticks_never_run_a_job_twice() {
    let Some(db) = db().await else { return };

    let setup = db.raw_client().await;
    setup
        .batch_execute("CREATE TABLE ran (job text PRIMARY KEY, token bigint NOT NULL)")
        .await
        .expect("create the ran table");

    let config = PgCoordinatorConfig::default();
    let nodes: Vec<Arc<Node>> = vec![
        Arc::new(node(&db, config).await),
        Arc::new(node(&db, config).await),
        Arc::new(node(&db, config).await),
    ];

    const JOBS: u32 = 12;
    const TICKS_PER_NODE: u32 = 700;
    // Short enough that a deliberately slow holder is overtaken by another node.
    let ttl = Duration::from_millis(300);
    let counts = Arc::new(Counts::default());

    let mut tasks = Vec::new();
    for (index, node) in nodes.iter().cloned().enumerate() {
        let counts = Arc::clone(&counts);
        tasks.push(tokio::spawn(async move {
            // A per-node xorshift, so the three nodes pick jobs differently.
            let mut state = 0x9E37_79B9u64 ^ ((index as u64 + 1) * 0x2545_F491_4F6C_DD1D);
            let holder = format!("node-{index}");
            for tick_index in 0..TICKS_PER_NODE {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let job = format!("job-{}", state % u64::from(JOBS));
                // Every 300th tick, hold past the ttl so another node takes over.
                let slow = tick_index % 300 == 299;
                tick(&node, &job, &holder, ttl, slow, &counts).await;
            }
        }));
    }
    for task in tasks {
        task.await.expect("a node task");
    }

    assert_eq!(
        counts.conflicts.load(Ordering::SeqCst),
        0,
        "a job was written by two holders"
    );
    assert!(
        counts.superseded.load(Ordering::SeqCst) >= 1,
        "no takeover was exercised; the fenced path is untested"
    );

    let ran: i64 = setup
        .query_one("SELECT count(*) FROM ran", &[])
        .await
        .expect("count ran")
        .get(0);
    assert_eq!(ran, i64::from(JOBS), "not every job ran exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_holder_hands_over_within_one_ttl() {
    let Some(db) = db().await else { return };
    let config = PgCoordinatorConfig::default();
    let dead = node(&db, config).await;
    let survivor = node(&db, config).await;

    let ttl = Duration::from_millis(300);
    let held = dead
        .coordinator
        .acquire("handover", "dead-node", ttl)
        .await
        .expect("the dead node takes the lease");
    assert_eq!(held.token, FencingToken(1));
    // The node dies: it drops its coordinator and never releases.
    drop(dead);
    drop(held);

    let start = Instant::now();
    let mut taken = None;
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(lease) = survivor
            .coordinator
            .acquire("handover", "survivor", ttl)
            .await
        {
            taken = Some(lease);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lease = taken.expect("the survivor took over");
    assert_eq!(lease.token, FencingToken(2));
    assert!(
        start.elapsed() < ttl + Duration::from_secs(2),
        "handover took {:?}, more than one ttl",
        start.elapsed()
    );
    survivor.coordinator.release(&lease).await.expect("release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rate_limit_is_never_exceeded_across_three_nodes() {
    let Some(db) = db().await else { return };
    let config = PgCoordinatorConfig::default();
    let nodes: Vec<Arc<PgCoordinator>> = vec![
        Arc::new(coordinator(&db, config, 8).await),
        Arc::new(coordinator(&db, config, 8).await),
        Arc::new(coordinator(&db, config, 8).await),
    ];
    let limit = RateLimit {
        capacity: 100,
        per: Duration::from_secs(1),
    };

    let mut tasks = Vec::new();
    let start = Instant::now();
    for node in &nodes {
        for _ in 0..100 {
            let node = Arc::clone(node);
            tasks.push(tokio::spawn(
                async move { node.take("shared", limit, 1).await },
            ));
        }
    }
    let mut admitted = 0u64;
    for task in tasks {
        if let Admit::Allowed { .. } = task.await.expect("a taker").expect("take") {
            admitted += 1;
        }
    }
    let elapsed = start.elapsed();
    // GCRA: 100 units burst, refilling one per 10ms. 300 racers take a little while to run, so
    // the bound is the burst plus the units that refilled; the point is it is nowhere near 300.
    let refilled = u64::try_from(elapsed.as_millis() / 10).unwrap_or(0) + 1;
    assert!(
        admitted <= 100 + refilled,
        "300 racing takers were admitted {admitted} times in {elapsed:?} over a 100/s limit"
    );
    assert!(
        admitted >= 100,
        "the limit admitted {admitted}, under its capacity"
    );
}
