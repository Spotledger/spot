//! The fence inside a business transaction, node presence and the cleanup job.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TTL, coordinator, db, node, pool, prepared};
use spot_coordinator::error::{LEASE_LOST, SECOND_NODE};
use spot_coordinator::{Coordinator, FencingToken, InProcessCoordinator, NodePresence};
use spot_coordinator_pg::{PgCoordinator, PgCoordinatorConfig, assert_fence};
use spot_core::ManualClock;
use time::OffsetDateTime;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_writer_is_refused_once_a_new_token_is_issued() {
    let Some(db) = db().await else { return };
    let c = prepared(&db).await;
    let mut client = db.raw_client().await;

    // Tokens 1 and 2 come and go; token 3 is live.
    for _ in 0..2 {
        let lease = c.acquire("job", "a", TTL).await.expect("acquire");
        c.release(&lease).await.expect("release");
    }
    let writer = c.acquire("job", "a", TTL).await.expect("acquire token 3");
    assert_eq!(writer.token, FencingToken(3));
    assert_fence(&client, "job", writer.token)
        .await
        .expect("token 3 is live");

    // Token 4 is issued after 3 expires: the token-3 writer is now stale.
    tokio::time::sleep(TTL + Duration::from_millis(100)).await;
    let next = c.acquire("job", "b", TTL).await.expect("acquire token 4");
    assert_eq!(next.token, FencingToken(4));
    assert_eq!(
        assert_fence(&client, "job", writer.token)
            .await
            .unwrap_err()
            .code,
        LEASE_LOST
    );
    assert_fence(&client, "job", next.token)
        .await
        .expect("token 4 is live");

    // The same check runs inside the writer's own transaction, holding the row with FOR SHARE.
    let tx = client.transaction().await.expect("begin");
    assert_eq!(
        assert_fence(&tx, "job", writer.token)
            .await
            .unwrap_err()
            .code,
        LEASE_LOST
    );
    assert_fence(&tx, "job", next.token)
        .await
        .expect("the live token fences the write");
    tx.rollback().await.expect("rollback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_registers_and_lists_other_nodes() {
    let Some(db) = db().await else { return };
    let c = prepared(&db).await;
    let presence = c.presence();

    let registration = presence.register(&node("node-a")).await.expect("register");
    assert!(presence.seen(&node("node-a")).await.expect("seen"));
    assert_eq!(
        presence.others(&node("node-b")).await.expect("others"),
        vec![node("node-a")]
    );
    assert!(
        presence
            .others(&node("node-a"))
            .await
            .expect("others")
            .is_empty()
    );

    presence
        .heartbeat(&node("node-a"))
        .await
        .expect("heartbeat");
    drop(registration);
    // The row is deleted in the background; poll briefly.
    for _ in 0..100 {
        if !presence.seen(&node("node-a")).await.expect("seen") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the node row was not withdrawn");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_process_coordinator_refuses_to_start_beside_a_live_node() {
    let Some(db) = db().await else { return };
    let c = prepared(&db).await;
    let presence = Arc::new(c.presence());

    // Another node is registered: the standalone profile must refuse to run.
    let _other = presence.register(&node("node-x")).await.expect("register");
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let error = match InProcessCoordinator::start(
        Arc::clone(&presence) as Arc<dyn NodePresence>,
        node("node-y"),
        clock,
    )
    .await
    {
        Ok(_) => panic!("started beside another live node"),
        Err(error) => error,
    };
    assert_eq!(error.code, SECOND_NODE);
    assert!(error.message.contains("node-x"), "{}", error.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_safe_from_several_nodes_at_once() {
    let Some(db) = db().await else { return };

    // Eight nodes build their coordinator and create the schema at the same instant. A bare
    // `CREATE TABLE IF NOT EXISTS` race would make some of them fail on a duplicate object.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let coordinator = PgCoordinator::new(pool(&db, 4), PgCoordinatorConfig::default());
        tasks.push(tokio::spawn(async move { coordinator.prepare().await }));
    }
    for task in tasks {
        task.await.expect("a prepare task").expect("prepare");
    }

    // The schema is usable afterwards: a lease taken here is the coordinator's.
    let c = prepared(&db).await;
    let lease = c.acquire("prepared", "a", TTL).await.expect("acquire");
    c.release(&lease).await.expect("release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_removes_expired_leases_dead_nodes_and_full_buckets() {
    let Some(db) = db().await else { return };
    // A short presence window so a node ages out quickly.
    let c = coordinator(
        &db,
        PgCoordinatorConfig::with_heartbeat(Duration::from_millis(100)),
        4,
    )
    .await;

    c.acquire("old", "a", Duration::from_millis(150))
        .await
        .expect("acquire");
    c.next("ctr2", 1).await.expect("counter");
    let _taken = c
        .take(
            "rl2",
            spot_coordinator::RateLimit {
                capacity: 1,
                per: Duration::from_millis(100),
            },
            1,
        )
        .await
        .expect("take");
    let _registration = c
        .presence()
        .register(&node("dead-node"))
        .await
        .expect("register");

    // Let the lease, the bucket and the node all go stale.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let report = c.cleanup().await.expect("cleanup");
    assert!(
        report.leases >= 1,
        "no expired lease was removed: {report:?}"
    );
    assert!(
        report.rate_buckets >= 1,
        "no full bucket was removed: {report:?}"
    );
    assert!(report.nodes >= 1, "no dead node was removed: {report:?}");

    assert!(c.current("old").await.expect("current").is_none());
    assert_eq!(
        c.peek("ctr2").await.expect("peek"),
        1,
        "cleanup moved a counter"
    );
    assert!(
        c.presence()
            .others(&node("me"))
            .await
            .expect("others")
            .is_empty()
    );
}
