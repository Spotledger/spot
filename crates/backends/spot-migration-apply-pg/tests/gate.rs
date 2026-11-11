//! The admission gate against a real database: fail-closed, the TTL cache, push invalidation
//! through the coordinator, the drain, and a second process seeing the close.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use spot_core::ManualClock;
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply::{GateStatus, MAINTENANCE_CODE};
use spot_migration_apply_pg::{PgAdmissionGate, PgApplySchema, UNREADABLE};
use support::*;
use time::OffsetDateTime;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH))
}

async fn gate(db: &spot_postgres::testing::TestDb, ttl: Duration) -> PgAdmissionGate {
    let pool = pool(db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    PgAdmissionGate::new(pool, site(), clock(), ttl)
}

#[tokio::test]
async fn a_gate_that_was_never_read_is_closed_not_open() {
    let Some(db) = db().await else { return };
    let gate = gate(&db, Duration::from_millis(50)).await;
    let status = gate.status();
    assert!(matches!(status, GateStatus::Closed { .. }), "{status:?}");
    let problem = gate.problem().expect("a closed gate has a problem");
    assert_eq!(problem.code, MAINTENANCE_CODE);
    assert_eq!(problem.detail, UNREADABLE);
    // A request path refreshes first: a site whose gate row is absent has never been closed
    // (the same "a fresh process has nothing to hold out" rule as the reference gate), so it
    // is served — but only after a successful read. A read that fails refuses (see below).
    assert!(admit(&gate).await.is_ok());
    assert_eq!(gate_label(&gate), "open");
}

#[tokio::test]
async fn close_and_open_are_seen_by_a_second_gate_on_the_same_row() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let writer = PgAdmissionGate::new(pool.clone(), site(), clock(), Duration::from_millis(50));
    let reader = PgAdmissionGate::new(pool, site(), clock(), Duration::from_millis(50));
    writer.refresh(1_000).await.unwrap();
    assert!(matches!(writer.status(), GateStatus::Open));

    writer.close("migration apply", 2_000).await.unwrap();
    assert_eq!(gate_label(&writer), "closed");
    // The other process's gate has no cache yet: it reads the row and sees closed.
    assert_eq!(gate_label(&reader), "closed");
    let status = reader.refresh(2_100).await.unwrap();
    assert!(matches!(
        status,
        GateStatus::Closed { ref reason, since: 2_000 } if reason == "migration apply"
    ));
    let problem = reader.problem().expect("closed");
    assert_eq!(problem.code, MAINTENANCE_CODE);
    assert_eq!(problem.since, 2_000);

    writer.open(3_000).await.unwrap();
    let status = reader.refresh(3_100).await.unwrap();
    assert_eq!(status, GateStatus::Open);
    assert!(reader.problem().is_none());
}

#[tokio::test]
async fn a_failed_read_closes_the_gate_even_after_it_was_open() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let gate = PgAdmissionGate::new(pool.clone(), site(), clock(), Duration::from_millis(50));
    gate.refresh(1_000).await.unwrap();
    assert_eq!(gate_label(&gate), "open");

    // Make the gate table unreadable from this role by dropping it: the refresh must fail and
    // the gate must answer closed.
    let client = pool.get().await.unwrap();
    client
        .batch_execute("DROP TABLE spot_migration_control.gate")
        .await
        .unwrap();
    assert!(gate.refresh(1_100).await.is_err());
    assert_eq!(gate_label(&gate), "closed");
    assert!(gate.problem().is_some());
    assert!(admit(&gate).await.is_err());
}

#[tokio::test]
async fn the_ttl_expires_the_cached_answer_and_a_refresh_sees_the_new_row() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let clock = clock();
    let gate = PgAdmissionGate::new(
        pool.clone(),
        site(),
        clock.clone(),
        Duration::from_millis(20),
    );
    gate.refresh(1_000).await.unwrap();
    assert!(gate.is_fresh());
    assert!(matches!(gate.status(), GateStatus::Open));

    // Another process closes the row; this gate's cache still says open until the TTL passes.
    let other = PgAdmissionGate::new(pool, site(), clock.clone(), Duration::from_millis(20));
    other.refresh(1_000).await.unwrap();
    other.close("elsewhere", 2_000).await.unwrap();
    assert_eq!(gate_label(&gate), "open");
    assert!(gate.age_micros().unwrap() < 20_000);

    clock.advance(Duration::from_millis(25));
    assert!(!gate.is_fresh());
    let status = gate.refresh_if_stale(2_100).await.unwrap();
    assert!(matches!(status, GateStatus::Closed { .. }));
    assert_eq!(gate_label(&gate), "closed");
}

#[tokio::test]
async fn a_coordinator_epoch_moves_the_cache_off_before_the_ttl() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let clock = clock();
    let coordinator = Arc::new(spot_coordinator_pg::PgCoordinator::new(
        pool.clone(),
        spot_coordinator_pg::PgCoordinatorConfig::default(),
    ));
    coordinator.prepare().await.unwrap();
    let gate = PgAdmissionGate::new(pool.clone(), site(), clock.clone(), Duration::from_secs(60))
        .with_coordinator(coordinator.clone());
    gate.refresh(1_000).await.unwrap();
    assert!(matches!(gate.status(), GateStatus::Open));

    // A second node writes the row and bumps the cluster-wide epoch; this node's next
    // `refresh_if_stale` must re-read even though its TTL has not expired.
    let writer = PgAdmissionGate::new(pool, site(), clock.clone(), Duration::from_secs(60))
        .with_coordinator(coordinator);
    writer.refresh(1_000).await.unwrap();
    writer.close("pushed", 2_000).await.unwrap();

    assert!(gate.is_fresh());
    let status = gate.refresh_if_stale(2_100).await.unwrap();
    assert!(matches!(status, GateStatus::Closed { .. }), "{status:?}");
}

#[tokio::test]
async fn invalidate_makes_the_next_answer_closed_until_a_refresh() {
    let Some(db) = db().await else { return };
    let gate = gate(&db, Duration::from_secs(60)).await;
    gate.refresh(1_000).await.unwrap();
    assert!(matches!(gate.status(), GateStatus::Open));
    gate.invalidate();
    assert!(matches!(gate.status(), GateStatus::Closed { .. }));
    assert!(admit(&gate).await.is_ok(), "a refresh restores the answer");
    assert!(matches!(gate.status(), GateStatus::Open));
}

#[tokio::test]
async fn the_drain_waits_for_another_connections_open_transaction() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 8);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let gate = PgAdmissionGate::new(pool.clone(), site(), clock(), Duration::from_millis(50));

    // A request path's transaction, on another connection, with a different application name.
    let held = pool_for(&db.config(), 2, "a-request-path");
    let mut client = held.get().await.unwrap();
    let tx = client.transaction().await.unwrap();
    tx.execute("SELECT 1", &[]).await.unwrap();
    assert!(gate.in_flight().await.unwrap() >= 1);

    let report = gate.drain(2_000_000, 1_000).await.unwrap();
    assert!(report.timed_out, "{report:?}");
    assert!(report.waited_micros >= 2_000_000);

    // Commit the transaction: the next drain returns at once.
    tx.commit().await.unwrap();
    let report = gate.drain(2_000_000, 2_000).await.unwrap();
    assert!(!report.timed_out, "{report:?}");
    assert_eq!(gate.in_flight().await.unwrap(), 0);
}
