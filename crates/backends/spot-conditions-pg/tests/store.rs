//! `PgConditionStore` against a real PostgreSQL database.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`; with
//! `SPOT_REQUIRE_PG=1` a missing server fails instead, as the sibling `*-pg` crates do.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_conditions::{AuditEvent, ConditionParams, Millis};
use spot_conditions_pg::PgConditionStore;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{Catalog, ChangeFeed, Collation, StorageBackend};

fn ms(millis: u64) -> Millis {
    Millis::new(millis)
}

struct Env {
    db: TestDb,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    // The feed tables (`spot_outbox` and friends) come from the document backend's own
    // `prepare`, not from this crate: a condition store never installs the outbox.
    backend.prepare(&catalog).await.unwrap();
    let store = PgConditionStore::new(backend.clone(), Arc::clone(&catalog));
    store.prepare().await.unwrap();
    Some(Env {
        db,
        backend,
        catalog,
    })
}

fn raised(id: &str, subject: &str, at: u64) -> AuditEvent {
    AuditEvent::Raised {
        id: spot_conditions::ConditionId::parse(id).unwrap(),
        subject: subject.into(),
        severity: spot_conditions::Severity::Amber,
        at_millis: ms(at),
        params: ConditionParams::new().with("count", "3"),
    }
}

fn cleared(id: &str, subject: &str, at: u64) -> AuditEvent {
    AuditEvent::Cleared {
        id: spot_conditions::ConditionId::parse(id).unwrap(),
        subject: subject.into(),
        severity: spot_conditions::Severity::Amber,
        at_millis: ms(at),
        params: ConditionParams::new().with("count", "3"),
    }
}

fn escalated(id: &str, subject: &str, at: u64) -> AuditEvent {
    AuditEvent::Escalated {
        id: spot_conditions::ConditionId::parse(id).unwrap(),
        subject: subject.into(),
        from: spot_conditions::Severity::Amber,
        to: spot_conditions::Severity::Red,
        at_millis: ms(at),
        params: ConditionParams::new().with("count", "9"),
    }
}

#[tokio::test]
async fn prepare_is_idempotent_and_load_open_starts_empty() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store.prepare().await.unwrap();
    assert!(store.load_open().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_raise_is_durable_across_a_restart() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 1_000)])
        .await
        .unwrap();

    // The "restart": a fresh store handle reads what the first one wrote.
    let restarted = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    let open = restarted.load_open().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id.as_str(), "NODE_SILENT");
    assert_eq!(open[0].subject.as_str(), "branch-12");
    assert_eq!(open[0].severity, spot_conditions::Severity::Amber);
    assert_eq!(open[0].params.get("count"), Some("3"));
    assert_eq!(open[0].raised_at_millis, ms(1_000));
    assert_eq!(open[0].last_seen_millis, ms(1_000));
}

#[tokio::test]
async fn an_escalation_updates_the_open_row_in_place() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 0)])
        .await
        .unwrap();
    store
        .apply(&[escalated("NODE_SILENT", "branch-12", 3_600_000)])
        .await
        .unwrap();

    let open = store.load_open().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].severity, spot_conditions::Severity::Red);
    // The row is still the one opened at 0: escalating never restarts `raised_at`.
    assert_eq!(open[0].raised_at_millis, ms(0));
    assert_eq!(open[0].last_seen_millis, ms(3_600_000));
}

#[tokio::test]
async fn a_clear_removes_the_open_row() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 0)])
        .await
        .unwrap();
    store
        .apply(&[cleared("NODE_SILENT", "branch-12", 70_000)])
        .await
        .unwrap();
    assert!(store.load_open().await.unwrap().is_empty());
}

#[tokio::test]
async fn apply_writes_the_matching_outbox_event_for_a_raise_and_a_clear() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 1_000)])
        .await
        .unwrap();
    store
        .apply(&[cleared("NODE_SILENT", "branch-12", 2_000)])
        .await
        .unwrap();

    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 2);

    assert_eq!(records[0].event.topic.as_str(), "sys_condition_raised");
    let raised_payload: serde_json::Value =
        serde_json::from_str(&records[0].event.payload).unwrap();
    assert_eq!(raised_payload["transition"], "raised");
    assert_eq!(raised_payload["id"], "NODE_SILENT");
    assert_eq!(raised_payload["subject"], "branch-12");
    assert_eq!(raised_payload["at_millis"], 1_000);
    assert_eq!(raised_payload["severity"], "amber");
    assert_eq!(raised_payload["params"]["count"], "3");
    assert!(raised_payload.get("from").is_none());

    assert_eq!(records[1].event.topic.as_str(), "sys_condition_cleared");
    let cleared_payload: serde_json::Value =
        serde_json::from_str(&records[1].event.payload).unwrap();
    assert_eq!(cleared_payload["transition"], "cleared");
    assert_eq!(cleared_payload["severity"], "amber");
}

#[tokio::test]
async fn an_escalation_outbox_event_carries_from_and_to_and_no_severity() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 0)])
        .await
        .unwrap();
    store
        .apply(&[escalated("NODE_SILENT", "branch-12", 3_600_000)])
        .await
        .unwrap();
    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 2);
    let payload: serde_json::Value = serde_json::from_str(&records[1].event.payload).unwrap();
    assert_eq!(payload["transition"], "escalated");
    assert_eq!(payload["from"], "amber");
    assert_eq!(payload["to"], "red");
    assert!(payload.get("severity").is_none());
}

#[tokio::test]
async fn history_records_every_transition_in_order() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 0)])
        .await
        .unwrap();
    store
        .apply(&[escalated("NODE_SILENT", "branch-12", 3_600_000)])
        .await
        .unwrap();
    store
        .apply(&[cleared("NODE_SILENT", "branch-12", 3_700_000)])
        .await
        .unwrap();

    let client = env.db.raw_client().await;
    let rows = client
        .query(
            "SELECT transition FROM spot_condition_history WHERE subject = 'branch-12' ORDER BY seq",
            &[],
        )
        .await
        .unwrap();
    let transitions: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(transitions, vec!["raised", "escalated", "cleared"]);
}

#[tokio::test]
async fn history_is_pruned_to_the_newest_1000_rows_per_subject() {
    let Some(env) = env().await else { return };
    let store = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    store
        .apply(&[raised("NODE_SILENT", "branch-12", 0)])
        .await
        .unwrap();

    // Seed 1,100 extra history rows directly (bypassing `apply`, which would be far slower for
    // a test): the next `apply` call runs the production prune over all of them.
    let client = env.db.raw_client().await;
    client
        .batch_execute(
            "INSERT INTO spot_condition_history (id, subject, transition, severity, params, at_millis) \
             SELECT 'NODE_SILENT', 'branch-12', 'escalated', 'amber', '{}'::jsonb, i \
             FROM generate_series(1, 1100) AS i",
        )
        .await
        .unwrap();

    store
        .apply(&[cleared("NODE_SILENT", "branch-12", 999_999)])
        .await
        .unwrap();

    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM spot_condition_history WHERE subject = 'branch-12'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1_000);
}

#[tokio::test]
async fn a_failure_partway_through_apply_writes_neither_the_open_row_nor_the_history() {
    let Some(env) = env().await else { return };
    // A write budget of 1 cannot fit the open-row write and the history-row write of a single
    // raise, let alone its outbox event: the transaction aborts before it can commit, forcing
    // the exact "kill between the history insert and the outbox insert" case the brief asks
    // for without any debug-only hook in the production path.
    let store = PgConditionStore::with_max_writes(env.backend.clone(), Arc::clone(&env.catalog), 1);
    let result = store.apply(&[raised("NODE_SILENT", "branch-12", 0)]).await;
    assert!(result.is_err());

    let verify = PgConditionStore::new(env.backend.clone(), Arc::clone(&env.catalog));
    assert!(verify.load_open().await.unwrap().is_empty());
    let client = env.db.raw_client().await;
    let count: i64 = client
        .query_one("SELECT count(*) FROM spot_condition_history", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn two_sites_never_see_each_others_conditions() {
    let Some(a) = env().await else { return };
    let Some(b) = env().await else { return };
    let store_a = PgConditionStore::new(a.backend.clone(), Arc::clone(&a.catalog));
    let store_b = PgConditionStore::new(b.backend.clone(), Arc::clone(&b.catalog));
    store_a
        .apply(&[raised("NODE_SILENT", "branch-12", 0)])
        .await
        .unwrap();
    assert_eq!(store_a.load_open().await.unwrap().len(), 1);
    assert!(store_b.load_open().await.unwrap().is_empty());
}
