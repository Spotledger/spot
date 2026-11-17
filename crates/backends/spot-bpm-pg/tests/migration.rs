//! `PgCaseStore::apply_migration` (WP-BPM-09.T7) on a real database: the move, the count and the
//! audit row land together; a failure after the Case write leaves the Case unmoved and uncounted;
//! every skip reason; a stale pin is skipped, never forced; two concurrent moves move a Case once;
//! another tenant's Case is never touched.
//!
//! Every test needs a server and skips cleanly (returning early) without one. The two migration
//! tables are created by the host (`spot-cli`), so the test creates them with the same DDL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::{Value as Json, json};
use spot_bpm_engine::{CaseId, CaseStore};
use spot_bpm_pg::{MigrationMove, MigrationOutcome, PgCaseStore, SkipReason};
use spot_postgres::testing::TestDb;

const HOST_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS spot_bpm_migration (\
  tenant text NOT NULL, id text NOT NULL, process text NOT NULL, from_version integer NOT NULL, \
  to_version integer NOT NULL, mapping jsonb NOT NULL, state text NOT NULL, \
  cursor text NOT NULL DEFAULT '', moved bigint NOT NULL DEFAULT 0, \
  skipped bigint NOT NULL DEFAULT 0, failed bigint NOT NULL DEFAULT 0, started_by text NOT NULL, \
  idem_key text, started_at timestamptz NOT NULL, ended_at timestamptz, lease_holder text, \
  lease_until timestamptz, PRIMARY KEY (tenant, id));\
CREATE TABLE IF NOT EXISTS spot_bpm_migration_audit (\
  tenant text NOT NULL, migration_id text NOT NULL, case_id text NOT NULL, \
  plan_digest text NOT NULL, payload text NOT NULL, at timestamptz NOT NULL, \
  PRIMARY KEY (tenant, case_id, plan_digest));";

const MIGRATION: &str = "mig-1";

/// A scratch database with the Case tables and the host's migration tables.
async fn setup() -> Option<(TestDb, PgCaseStore)> {
    let db = TestDb::create().await?;
    let store = PgCaseStore::new(pool(&db, 6, "bpm-migration")).with_tenant("site_a");
    store.prepare().await.unwrap();
    db.raw_client()
        .await
        .batch_execute(HOST_TABLES)
        .await
        .unwrap();
    Some((db, store))
}

/// A migration record of `tenant` on `process` from v1 to v2.
async fn open_migration(db: &TestDb, tenant: &str) {
    db.raw_client()
        .await
        .execute(
            "INSERT INTO spot_bpm_migration (tenant, id, process, from_version, to_version, \
             mapping, state, started_by, started_at) \
             VALUES ($1, $2, 'chain', 1, 2, '{}'::jsonb, 'running', 'ops', now())",
            &[&tenant, &MIGRATION],
        )
        .await
        .unwrap();
}

/// A running Case of `chain` v1 in `store`'s tenant; returns its stored revision.
async fn start_case(db: &TestDb, store: &PgCaseStore, id: &str) -> i64 {
    store
        .create(new_case(&chain(2), id, &format!("key-{id}")))
        .await
        .unwrap();
    revision_of(db, id).await
}

async fn revision_of(db: &TestDb, id: &str) -> i64 {
    db.raw_client()
        .await
        .query_one(
            "SELECT version FROM spot_bpm_case WHERE case_id = $1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

/// What the Case row says: `(dag_version, content_hash)`.
async fn pin(db: &TestDb, id: &str) -> (i32, String) {
    let row = db
        .raw_client()
        .await
        .query_one(
            "SELECT dag_version, content_hash FROM spot_bpm_case WHERE case_id = $1",
            &[&id],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn count(db: &TestDb, statement: &str) -> i64 {
    db.raw_client()
        .await
        .query_one(statement, &[])
        .await
        .unwrap()
        .get(0)
}

async fn moved(db: &TestDb) -> i64 {
    count(db, "SELECT moved FROM spot_bpm_migration").await
}

async fn audit_rows(db: &TestDb) -> i64 {
    count(db, "SELECT count(*) FROM spot_bpm_migration_audit").await
}

/// The move of `case` the plan made at `revision`: v1 to v2.
struct Planned {
    case: CaseId,
    nodes: Json,
    taken: Json,
    variables: Json,
    revision: u64,
    digest: String,
}

impl Planned {
    fn new(id: &str, revision: i64) -> Self {
        let dag = chain(2);
        let record = new_case(&dag, id, "k");
        Self {
            case: CaseId::new(id).unwrap(),
            nodes: serde_json::to_value(&record.run.nodes).unwrap(),
            taken: json!([]),
            variables: json!({"migrated": true}),
            revision: u64::try_from(revision).unwrap(),
            digest: format!("digest-{id}"),
        }
    }

    fn as_move(&self) -> MigrationMove<'_> {
        MigrationMove {
            case: &self.case,
            from_name: "chain",
            from_version: 1,
            expected_revision: self.revision,
            to_version: 2,
            to_hash: "hash-v2",
            nodes: &self.nodes,
            taken: &self.taken,
            has_ready: true,
            variables: &self.variables,
            migration_id: MIGRATION,
            plan_digest: &self.digest,
            audit_payload: "{\"topic\":\"bpm.migration\"}",
            at: now(),
        }
    }
}

#[tokio::test]
async fn a_run_moves_and_the_count_and_the_audit_row_land_together() {
    let Some((db, store)) = setup().await else {
        return;
    };
    open_migration(&db, "site_a").await;
    let revision = start_case(&db, &store, "c1").await;
    let planned = Planned::new("c1", revision);
    let outcome = store.apply_migration(&planned.as_move()).await.unwrap();
    assert_eq!(outcome, MigrationOutcome::Moved);
    assert_eq!(pin(&db, "c1").await, (2, "hash-v2".to_owned()));
    assert_eq!(revision_of(&db, "c1").await, revision + 1);
    assert_eq!(moved(&db).await, 1);
    assert_eq!(audit_rows(&db).await, 1);
    let variables: Json = db
        .raw_client()
        .await
        .query_one(
            "SELECT variables FROM spot_bpm_case_var WHERE case_id = 'c1'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(variables, json!({"migrated": true}));
}

#[tokio::test]
async fn a_failure_after_the_run_write_leaves_it_unmoved_and_uncounted() {
    let Some((db, store)) = setup().await else {
        return;
    };
    open_migration(&db, "site_a").await;
    let revision = start_case(&db, &store, "c1").await;
    let variables_before = count(&db, "SELECT count(*) FROM spot_bpm_case_var").await;
    let planned = Planned::new("c1", revision);
    // The audit insert fails (its key already exists), after the Case and variable writes.
    db.raw_client()
        .await
        .execute(
            "INSERT INTO spot_bpm_migration_audit (tenant, migration_id, case_id, plan_digest, payload, at) \
             VALUES ('site_a', $1, 'c1', $2, '{}', now())",
            &[&MIGRATION, &planned.digest],
        )
        .await
        .unwrap();
    let error = store.apply_migration(&planned.as_move()).await;
    assert!(error.is_err(), "{error:?}");
    assert_eq!(pin(&db, "c1").await.0, 1);
    assert_eq!(revision_of(&db, "c1").await, revision);
    assert_eq!(moved(&db).await, 0);
    assert_eq!(audit_rows(&db).await, 1, "only the pre-seeded row");
    // The Case's own variable row (made when it started) is as it was: the move wrote none.
    assert_eq!(
        count(&db, "SELECT count(*) FROM spot_bpm_case_var").await,
        variables_before
    );
    let migrated: i64 = count(
        &db,
        "SELECT count(*) FROM spot_bpm_case_var WHERE variables @> '{\"migrated\": true}'::jsonb",
    )
    .await;
    assert_eq!(migrated, 0, "the migrated variables were rolled back");
}

#[tokio::test]
async fn a_missing_migration_record_rolls_the_move_back() {
    let Some((db, store)) = setup().await else {
        return;
    };
    let revision = start_case(&db, &store, "c1").await;
    let planned = Planned::new("c1", revision);
    assert!(store.apply_migration(&planned.as_move()).await.is_err());
    assert_eq!(pin(&db, "c1").await.0, 1);
    assert_eq!(audit_rows(&db).await, 0);
}

/// Runs the move and asserts it was skipped for `reason` with nothing written.
async fn assert_skipped(
    db: &TestDb,
    store: &PgCaseStore,
    planned: &Planned,
    reason: SkipReason,
    pinned: i32,
) {
    let outcome = store.apply_migration(&planned.as_move()).await.unwrap();
    let missing = reason == SkipReason::NotFound;
    assert_eq!(outcome, MigrationOutcome::Skipped(reason));
    if !missing {
        // (A Case that does not exist has no pin to read.)
        assert_eq!(pin(db, planned.case.as_str()).await.0, pinned);
    }
    assert_eq!(moved(db).await, 0);
    assert_eq!(audit_rows(db).await, 0);
}

#[tokio::test]
async fn each_skip_reason_is_reported_and_writes_nothing() {
    let Some((db, store)) = setup().await else {
        return;
    };
    open_migration(&db, "site_a").await;
    let raw = db.raw_client().await;

    let planned = Planned::new("nobody", 0);
    assert_skipped(&db, &store, &planned, SkipReason::NotFound, 1).await;

    let revision = start_case(&db, &store, "c_stale").await;
    let stale = Planned::new("c_stale", revision + 1);
    assert_skipped(&db, &store, &stale, SkipReason::ChangedSincePlan, 1).await;

    start_case(&db, &store, "c_done").await;
    raw.execute(
        "UPDATE spot_bpm_case SET status = 'completed' WHERE case_id = 'c_done'",
        &[],
    )
    .await
    .unwrap();
    let done = Planned::new("c_done", revision_of(&db, "c_done").await);
    assert_skipped(&db, &store, &done, SkipReason::NotRunning, 1).await;

    start_case(&db, &store, "c_mid").await;
    raw.execute(
        "UPDATE spot_bpm_case SET nodes = jsonb_set(nodes, '{n0,status}', '\"running\"') WHERE case_id = 'c_mid'",
        &[],
    )
    .await
    .unwrap();
    let mid = Planned::new("c_mid", revision_of(&db, "c_mid").await);
    assert_skipped(&db, &store, &mid, SkipReason::MidNode, 1).await;

    start_case(&db, &store, "c_attempt").await;
    raw.execute(
        "INSERT INTO spot_bpm_node_outcome (case_id, tenant, node, attempt, state, started_at) \
         VALUES ('c_attempt', 'site_a', 'n0', 1, 'started', now())",
        &[],
    )
    .await
    .unwrap();
    let attempt = Planned::new("c_attempt", revision_of(&db, "c_attempt").await);
    assert_skipped(&db, &store, &attempt, SkipReason::MidNode, 1).await;

    start_case(&db, &store, "c_wait").await;
    raw.execute(
        "INSERT INTO spot_bpm_node_outcome (case_id, tenant, node, attempt, state, started_at, event) \
         VALUES ('c_wait', 'site_a', 'n0', 1, 'waiting', now(), 'order.paid')",
        &[],
    )
    .await
    .unwrap();
    let wait = Planned::new("c_wait", revision_of(&db, "c_wait").await);
    assert_skipped(&db, &store, &wait, SkipReason::Waiting, 1).await;

    start_case(&db, &store, "c_child").await;
    raw.execute(
        "UPDATE spot_bpm_case SET has_children = true WHERE case_id = 'c_child'",
        &[],
    )
    .await
    .unwrap();
    let child = Planned::new("c_child", revision_of(&db, "c_child").await);
    assert_skipped(&db, &store, &child, SkipReason::Waiting, 1).await;
}

#[tokio::test]
async fn a_stale_expected_version_is_skipped_never_forced() {
    let Some((db, store)) = setup().await else {
        return;
    };
    open_migration(&db, "site_a").await;
    let revision = start_case(&db, &store, "c1").await;
    let planned = Planned::new("c1", revision);
    let mut mv = planned.as_move();
    mv.from_version = 3;
    let outcome = store.apply_migration(&mv).await.unwrap();
    assert_eq!(outcome, MigrationOutcome::Skipped(SkipReason::NotOnVersion));
    mv.from_version = 1;
    mv.from_name = "another_process";
    let outcome = store.apply_migration(&mv).await.unwrap();
    assert_eq!(outcome, MigrationOutcome::Skipped(SkipReason::NotOnVersion));
    assert_eq!(pin(&db, "c1").await.0, 1);
    assert_eq!(moved(&db).await, 0);
}

#[tokio::test]
async fn two_concurrent_moves_of_one_run_move_it_once() {
    let Some((db, store)) = setup().await else {
        return;
    };
    open_migration(&db, "site_a").await;
    let revision = start_case(&db, &store, "c1").await;
    let planned = Planned::new("c1", revision);
    let mut second = Planned::new("c1", revision);
    second.digest = "digest-other".to_owned();
    let (first_move, second_move) = (planned.as_move(), second.as_move());
    let (a, b) = tokio::join!(
        store.apply_migration(&first_move),
        store.apply_migration(&second_move)
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    let wins = outcomes
        .iter()
        .filter(|o| **o == MigrationOutcome::Moved)
        .count();
    assert_eq!(wins, 1, "{outcomes:?}");
    // The loser re-read the Case under the lock: it is on v2 now, or its revision moved on.
    assert!(outcomes.iter().any(|o| matches!(
        o,
        MigrationOutcome::Skipped(SkipReason::NotOnVersion | SkipReason::ChangedSincePlan)
    )));
    assert_eq!(moved(&db).await, 1);
    assert_eq!(audit_rows(&db).await, 1);
    assert_eq!(pin(&db, "c1").await.0, 2);
}

#[tokio::test]
async fn a_run_of_another_site_is_untouched() {
    let Some((db, store_a)) = setup().await else {
        return;
    };
    let store_b = store_a.clone().with_tenant("site_b");
    open_migration(&db, "site_a").await;
    let revision = start_case(&db, &store_b, "c_b").await;
    let planned = Planned::new("c_b", revision);
    let outcome = store_a.apply_migration(&planned.as_move()).await.unwrap();
    assert_eq!(outcome, MigrationOutcome::Skipped(SkipReason::NotFound));
    assert_eq!(pin(&db, "c_b").await.0, 1);
    assert_eq!(revision_of(&db, "c_b").await, revision);
    assert_eq!(moved(&db).await, 0);
    assert_eq!(audit_rows(&db).await, 0);
}

#[tokio::test]
async fn the_version_counts_and_the_id_pages_are_tenant_scoped() {
    let Some((db, store_a)) = setup().await else {
        return;
    };
    let store_b = store_a.clone().with_tenant("site_b");
    for id in ["c1", "c2", "c3"] {
        start_case(&db, &store_a, id).await;
    }
    start_case(&db, &store_b, "other").await;
    assert_eq!(
        store_a.running_by_version("chain").await.unwrap(),
        vec![(1, 3)]
    );
    assert_eq!(
        store_b.running_by_version("chain").await.unwrap(),
        vec![(1, 1)]
    );
    let first = store_a.running_case_ids("chain", 1, "", 2).await.unwrap();
    assert_eq!(first, vec!["c1".to_owned(), "c2".to_owned()]);
    let rest = store_a.running_case_ids("chain", 1, "c2", 2).await.unwrap();
    assert_eq!(rest, vec!["c3".to_owned()]);
    assert!(
        store_a
            .running_case_ids("chain", 2, "", 2)
            .await
            .unwrap()
            .is_empty()
    );
}
