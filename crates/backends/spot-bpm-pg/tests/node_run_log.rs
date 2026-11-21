//! The PostgreSQL node-run transition log (WP-BPM-11.T6) on a real database: the engine's contract
//! test, crash atomicity, ordering under concurrency, tenant isolation, the checkpoint
//! compare-and-set, pruning and the idempotent schema.
//!
//! Every test needs a server and skips cleanly (returning early) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use deadpool_postgres::Pool;
use serde_json::json;
use spot_bpm_engine::{
    Attempt, AttemptState, CaseId, CaseStore, CaseUpdate, Checkpoint, LoggedCaseStore, LoggedState,
    LoggedWait, NodeRunLog, NodeRunLogFactory, NodeRunRecord, NodeRunSeq, TimerRecord,
    node_run_log_contract,
};
use spot_bpm_pg::{PgCaseStore, sql};
use spot_dag_exec::{Instant, NodeOutcome, claim, complete};
use spot_postgres::testing::TestDb;

/// A factory that opens a [`PgCaseStore`] per tenant over one pool.
struct PgFactory {
    pool: Pool,
}

impl NodeRunLogFactory for PgFactory {
    fn open(&self, tenant: &str) -> Box<dyn LoggedCaseStore> {
        Box::new(PgCaseStore::new(self.pool.clone()).with_tenant(tenant))
    }
}

/// An instant `seconds` after the epoch.
fn at(seconds: i64) -> Instant {
    Instant { seconds, nanos: 0 }
}

/// A prepared store for `tenant`.
async fn store(db: &TestDb, tenant: &str, size: usize) -> PgCaseStore {
    let store = PgCaseStore::new(pool(db, size, "bpm-log")).with_tenant(tenant);
    store.prepare().await.unwrap();
    store
}

/// A Case with `rounds` started attempts of `n0`: `rounds` log records.
async fn fill(store: &PgCaseStore, id: &str, key: &str, rounds: u32) {
    let dag = chain(2);
    let record = store.create(new_case(&dag, id, key)).await.unwrap();
    let case = CaseId::new(id).unwrap();
    for round in 1..=rounds {
        let second = i64::from(round);
        store
            .commit(
                &case,
                record.run.version,
                CaseUpdate::new()
                    .with_attempt(Attempt::started(nid("n0"), round, at(second), false))
                    .with_at(at(second)),
            )
            .await
            .unwrap();
    }
}

/// The number of log rows of `tenant`, read raw.
async fn log_rows(db: &TestDb, tenant: &str) -> i64 {
    db.raw_client()
        .await
        .query_one(
            "SELECT count(*) FROM spot_bpm_node_run_event WHERE tenant = $1",
            &[&tenant],
        )
        .await
        .unwrap()
        .get(0)
}

/// A single `bigint` from a raw query.
async fn scalar(db: &TestDb, statement: &str) -> i64 {
    db.raw_client()
        .await
        .query_one(statement, &[])
        .await
        .unwrap()
        .get(0)
}

/// Every row of the three log tables as canonical JSON text, in a fixed order.
async fn dump_log(db: &TestDb) -> Vec<String> {
    let client = db.raw_client().await;
    let mut out = Vec::new();
    for table in ["event", "seq", "consumer"] {
        let rows = client
            .query(
                &format!("SELECT to_jsonb(t)::text FROM spot_bpm_node_run_{table} t ORDER BY 1"),
                &[],
            )
            .await
            .unwrap();
        out.extend(rows.iter().map(|r| r.get::<_, String>(0)));
    }
    out
}

/// The relations (tables, indexes) the log created, sorted.
async fn log_objects(db: &TestDb) -> Vec<String> {
    db.raw_client()
        .await
        .query(
            "SELECT relname::text FROM pg_class \
             WHERE relnamespace = to_regnamespace(current_schema())::oid \
               AND relname LIKE 'spot\\_bpm\\_node\\_run\\_%' ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect()
}

fn seqs(records: &[NodeRunRecord]) -> Vec<i64> {
    records.iter().map(|r| r.seq.0).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_contract_holds_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4, "bpm-log-contract");
    PgCaseStore::new(pool.clone()).prepare().await.unwrap();
    node_run_log_contract(&PgFactory { pool }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_commit_leaves_neither_node_rows_nor_log_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 4).await;
    let dag = chain(3);
    let record = store
        .create(new_case(&dag, "atomic", "atomic-key"))
        .await
        .unwrap();
    let case = CaseId::new("atomic").unwrap();
    let step = claim(&record.run, &nid("n0"), now()).unwrap();
    let record = store
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new().with_transition(step),
        )
        .await
        .unwrap();
    assert_eq!(log_rows(&db, "default").await, 1);
    let counter = scalar(&db, "SELECT last FROM spot_bpm_node_run_seq").await;
    assert_eq!(counter, 1);

    // The log insert is the last write of the commit; make it fail after the node rows, the
    // variables and the Case row were written.
    let client = db.raw_client().await;
    client
        .batch_execute(
            "CREATE FUNCTION spot_test_refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'refused'; END $$; \
             CREATE TRIGGER spot_test_refuse BEFORE INSERT ON spot_bpm_node_run_event \
             FOR EACH ROW EXECUTE FUNCTION spot_test_refuse();",
        )
        .await
        .unwrap();
    let update = || {
        CaseUpdate::new()
            .with_attempt(Attempt::started(nid("n0"), 2, at(20), true))
            .with_variables(json!({ "k": 1 }))
            .with_at(at(20))
    };
    assert!(
        store
            .commit(&case, record.run.version, update())
            .await
            .is_err()
    );
    let after = store.load(&case).await.unwrap().unwrap();
    assert!(
        after.attempts.is_empty(),
        "no node row survived the failure"
    );
    assert_eq!(after.run.version, record.run.version);
    assert_ne!(
        after.variables,
        json!({ "k": 1 }),
        "the variables rolled back"
    );
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM spot_bpm_node_outcome").await,
        0
    );
    assert_eq!(
        log_rows(&db, "default").await,
        1,
        "no log row for the failed commit"
    );
    assert_eq!(
        scalar(&db, "SELECT last FROM spot_bpm_node_run_seq").await,
        counter,
        "the counter rolled back too"
    );

    // Without the fault the same commit leaves both.
    client
        .batch_execute("DROP TRIGGER spot_test_refuse ON spot_bpm_node_run_event")
        .await
        .unwrap();
    store
        .commit(&case, record.run.version, update())
        .await
        .unwrap();
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM spot_bpm_node_outcome").await,
        1
    );
    assert_eq!(log_rows(&db, "default").await, 2);
    assert_eq!(
        scalar(&db, "SELECT last FROM spot_bpm_node_run_seq").await,
        2
    );
    let records = store.list_after(NodeRunSeq(0), 10).await.unwrap();
    assert_eq!(seqs(&records), vec![1, 2]);
    assert_eq!(records[1].attempt, 2);
    assert!(records[1].is_retry);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_commits_give_a_gapless_log_a_polling_reader_never_skips() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    const CASES: u32 = 24;
    const ROUNDS: u32 = 8;
    let store = store(&db, "storm", 16).await;
    let dag = chain(2);
    let mut versions = Vec::new();
    for case in 0..CASES {
        let record = store
            .create(new_case(
                &dag,
                &format!("storm_{case}"),
                &format!("storm-key-{case}"),
            ))
            .await
            .unwrap();
        versions.push(record.run.version);
    }
    let total = usize::try_from(CASES * ROUNDS).unwrap();

    // The reader polls while the writers run, a page of 7 at a time, from its own cursor.
    let reader_store = store.clone();
    let reader = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut seen: Vec<NodeRunRecord> = Vec::new();
        let mut cursor = NodeRunSeq(0);
        while seen.len() < total {
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "the reader timed out"
            );
            let page = reader_store.list_after(cursor, 7).await.unwrap();
            if let Some(last) = page.last() {
                cursor = last.seq;
                seen.extend(page);
            } else {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        seen
    });
    let mut writers = Vec::new();
    for case in 0..CASES {
        let writer = store.clone();
        let version = versions[usize::try_from(case).unwrap()];
        writers.push(tokio::spawn(async move {
            let id = CaseId::new(&format!("storm_{case}")).unwrap();
            for round in 1..=ROUNDS {
                let second = i64::from(round);
                writer
                    .commit(
                        &id,
                        version,
                        CaseUpdate::new()
                            .with_attempt(Attempt::started(nid("n0"), round, at(second), false))
                            .with_at(at(second)),
                    )
                    .await
                    .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    let seen = reader.await.unwrap();

    // The reader saw every number once, in order: nothing was skipped while it polled.
    let expected: Vec<i64> = (1..=i64::try_from(total).unwrap()).collect();
    assert_eq!(seqs(&seen), expected);
    assert_eq!(
        scalar(&db, "SELECT last FROM spot_bpm_node_run_seq").await,
        i64::try_from(expected.len()).unwrap()
    );
    // A fresh full read agrees with it.
    let mut all = Vec::new();
    let mut cursor = NodeRunSeq(0);
    loop {
        let page = store.list_after(cursor, 1000).await.unwrap();
        let Some(last) = page.last() else { break };
        cursor = last.seq;
        all.extend(page);
    }
    assert_eq!(all, seen);
    // Within one Case the records follow the order its commits happened.
    for case in 0..CASES {
        let id = format!("storm_{case}");
        let attempts: Vec<u32> = seen
            .iter()
            .filter(|r| r.case.as_str() == id)
            .map(|r| r.attempt)
            .collect();
        assert_eq!(attempts, (1..=ROUNDS).collect::<Vec<_>>(), "{id}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_commits_of_one_case_appear_in_version_order_and_a_racing_claim_logs_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 12).await;
    let dag = chain(3);
    let id = CaseId::new("ordered").unwrap();
    let mut record = store
        .create(new_case(&dag, "ordered", "ordered-key"))
        .await
        .unwrap();

    // Eight sessions race to claim the same node at the same version: one advances the Case, the
    // others are the idempotent replay, and the log holds a single `started`.
    let step = claim(&record.run, &nid("n0"), now()).unwrap();
    let version = record.run.version;
    let mut racers = Vec::new();
    for _ in 0..8 {
        let racer = store.clone();
        let id = id.clone();
        let step = step.clone();
        racers.push(tokio::spawn(async move {
            racer
                .commit(&id, version, CaseUpdate::new().with_transition(step))
                .await
        }));
    }
    for racer in racers {
        racer.await.unwrap().unwrap();
    }
    assert_eq!(log_rows(&db, "default").await, 1);

    record = store.load(&id).await.unwrap().unwrap();
    let finish = complete(
        &dag,
        &record.run,
        &nid("n0"),
        NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        now(),
    )
    .unwrap();
    record = store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new().with_transition(finish),
        )
        .await
        .unwrap();
    let step = claim(&record.run, &nid("n1"), now()).unwrap();
    store
        .commit(
            &id,
            record.run.version,
            CaseUpdate::new().with_transition(step),
        )
        .await
        .unwrap();
    let records = store.list_after(NodeRunSeq(0), 100).await.unwrap();
    assert_eq!(seqs(&records), vec![1, 2, 3]);
    // The case is created at version 1; each commit below raises it by one.
    assert_eq!(
        records.iter().map(|r| r.case_version).collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert_eq!(
        records.iter().map(|r| r.state).collect::<Vec<_>>(),
        vec![
            LoggedState::Started,
            LoggedState::Done,
            LoggedState::Started
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenants_never_see_each_others_records_counters_or_checkpoints() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let alpha = store(&db, "alpha", 4).await;
    let beta = PgCaseStore::new(pool(&db, 4, "bpm-log-beta")).with_tenant("beta");
    let crafted =
        PgCaseStore::new(pool(&db, 2, "bpm-log-crafted")).with_tenant("alpha' OR tenant <> '");
    fill(&alpha, "alpha_case", "alpha-key", 3).await;
    fill(&beta, "beta_case", "beta-key", 2).await;

    let a = alpha.list_after(NodeRunSeq(0), 100).await.unwrap();
    let b = beta.list_after(NodeRunSeq(0), 100).await.unwrap();
    assert_eq!(seqs(&a), vec![1, 2, 3]);
    assert_eq!(seqs(&b), vec![1, 2], "each tenant counts from 1");
    assert!(a.iter().all(|r| r.case.as_str() == "alpha_case"));
    assert!(b.iter().all(|r| r.case.as_str() == "beta_case"));
    assert!(
        crafted
            .list_after(NodeRunSeq(0), 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM spot_bpm_node_run_seq").await,
        2,
        "one counter per tenant"
    );

    // The same consumer name is a different checkpoint in each tenant.
    let one = Checkpoint {
        seq: NodeRunSeq(1),
        state: json!({ "who": "alpha" }),
    };
    assert!(
        alpha
            .save_checkpoint("insight", NodeRunSeq(0), one.clone())
            .await
            .unwrap()
    );
    assert!(beta.load_checkpoint("insight").await.unwrap().is_none());
    assert!(crafted.load_checkpoint("insight").await.unwrap().is_none());
    let two = Checkpoint {
        seq: NodeRunSeq(2),
        state: json!({ "who": "beta" }),
    };
    assert!(
        beta.save_checkpoint("insight", NodeRunSeq(0), two.clone())
            .await
            .unwrap()
    );
    assert_eq!(alpha.load_checkpoint("insight").await.unwrap(), Some(one));
    assert_eq!(beta.load_checkpoint("insight").await.unwrap(), Some(two));
    // A consumer named like the other tenant is still this tenant's own row.
    assert!(alpha.load_checkpoint("beta").await.unwrap().is_none());

    // Crafted consumer names never reach SQL.
    for name in [
        "x' OR '1'='1",
        "a;DROP TABLE spot_bpm_node_run_event",
        "Upper",
        "",
    ] {
        assert!(alpha.load_checkpoint(name).await.is_err(), "{name:?}");
        assert!(
            alpha
                .save_checkpoint(
                    name,
                    NodeRunSeq(0),
                    Checkpoint {
                        seq: NodeRunSeq(0),
                        state: json!(null)
                    }
                )
                .await
                .is_err(),
            "{name:?}"
        );
    }

    // A prune is one tenant's: the crafted tenant, and beta, remove nothing of alpha's.
    assert_eq!(crafted.prune_before(NodeRunSeq(1000)).await.unwrap(), 0);
    assert_eq!(beta.prune_before(NodeRunSeq(1000)).await.unwrap(), 2);
    assert_eq!(log_rows(&db, "alpha").await, 3);
    assert_eq!(log_rows(&db, "beta").await, 0);

    // Every statement of the log is tenant-scoped, by a bound parameter.
    for statement in sql::NODE_RUN_STATEMENTS {
        assert!(
            statement.contains("tenant = $1") || statement.contains("(tenant, "),
            "no tenant predicate: {statement}"
        );
        assert!(!statement.contains('\''), "a literal in: {statement}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_checkpoint_compare_and_set_has_one_winner_and_never_moves_backwards() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 16).await;
    let first = race(&store, NodeRunSeq(0)).await;
    assert_eq!(first.len(), 1, "exactly one of the racing first saves wins");
    let winner = first[0];
    let stored = store.load_checkpoint("insight").await.unwrap().unwrap();
    assert_eq!(stored.seq, NodeRunSeq(winner));
    assert_eq!(stored.state, json!({ "writer": winner }));

    let second = race(&store, stored.seq).await;
    assert_eq!(second.len(), 1, "exactly one of the racing updates wins");
    let now_stored = store.load_checkpoint("insight").await.unwrap().unwrap();
    assert_eq!(now_stored.seq, NodeRunSeq(second[0]));
    assert!(now_stored.seq > stored.seq);

    // A stale expected position loses without changing anything; a backwards move is refused.
    assert!(
        !store
            .save_checkpoint(
                "insight",
                stored.seq,
                Checkpoint {
                    seq: NodeRunSeq(stored.seq.0 + 1000),
                    state: json!(null)
                }
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .save_checkpoint(
                "insight",
                now_stored.seq,
                Checkpoint {
                    seq: NodeRunSeq(now_stored.seq.0 - 1),
                    state: json!(null)
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.load_checkpoint("insight").await.unwrap(),
        Some(now_stored)
    );
}

/// Sixteen writers save `expected -> expected + 1 + i` at once; returns the winning `seq`s.
async fn race(store: &PgCaseStore, expected: NodeRunSeq) -> Vec<i64> {
    let mut tasks = Vec::new();
    for writer in 0..16_i64 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let next = Checkpoint {
                seq: NodeRunSeq(expected.0 + 1 + writer),
                state: json!({ "writer": expected.0 + 1 + writer }),
            };
            store
                .save_checkpoint("insight", expected, next)
                .await
                .unwrap()
        }));
    }
    let mut winners = Vec::new();
    for (writer, task) in tasks.into_iter().enumerate() {
        if task.await.unwrap() {
            winners.push(expected.0 + 1 + i64::try_from(writer).unwrap());
        }
    }
    winners
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_never_removes_a_record_a_consumer_has_not_folded() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 4).await;
    fill(&store, "prune_case", "prune-key", 6).await;
    let save = |consumer: &'static str, expected: i64, seq: i64| {
        let store = store.clone();
        async move {
            store
                .save_checkpoint(
                    consumer,
                    NodeRunSeq(expected),
                    Checkpoint {
                        seq: NodeRunSeq(seq),
                        state: json!({}),
                    },
                )
                .await
                .unwrap()
        }
    };
    assert!(save("fast", 0, 4).await);
    assert!(save("slow", 0, 2).await);
    // Asked to remove everything, only what the slowest consumer has folded goes.
    assert_eq!(store.prune_before(NodeRunSeq(1000)).await.unwrap(), 2);
    assert_eq!(
        seqs(&store.list_after(NodeRunSeq(0), 100).await.unwrap()),
        vec![3, 4, 5, 6]
    );
    assert!(save("slow", 2, 4).await);
    // A cut below the slowest checkpoint is honoured as asked.
    assert_eq!(store.prune_before(NodeRunSeq(4)).await.unwrap(), 1);
    assert_eq!(store.prune_before(NodeRunSeq(1000)).await.unwrap(), 1);
    assert_eq!(
        seqs(&store.list_after(NodeRunSeq(0), 100).await.unwrap()),
        vec![5, 6]
    );
    // The counter is not reset: the next record continues after the pruned ones.
    let dag = chain(2);
    let record = store
        .create(new_case(&dag, "after_prune", "after-prune-key"))
        .await
        .unwrap();
    store
        .commit(
            &CaseId::new("after_prune").unwrap(),
            record.run.version,
            CaseUpdate::new()
                .with_attempt(Attempt::started(nid("n0"), 1, at(1), false))
                .with_at(at(1)),
        )
        .await
        .unwrap();
    assert_eq!(
        seqs(&store.list_after(NodeRunSeq(0), 100).await.unwrap()),
        vec![5, 6, 7]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_log_holds_codes_and_instants_never_payloads() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 4).await;
    let dag = chain(2);
    let record = store
        .create(new_case(&dag, "secret_case", "secret-key"))
        .await
        .unwrap();
    let id = CaseId::new("secret_case").unwrap();
    let version = record.run.version;
    let mut done = Attempt::started(nid("n0"), 1, at(1), false);
    done.state = AttemptState::Done;
    done.output = Some(json!({ "secret": "SENTINEL-OUTPUT" }));
    store
        .commit(
            &id,
            version,
            CaseUpdate::new()
                .with_attempt(done)
                .with_variables(json!({ "v": "SENTINEL-VARIABLE" }))
                .with_at(at(2)),
        )
        .await
        .unwrap();
    let mut failed = Attempt::started(nid("n1"), 1, at(3), false);
    failed.state = AttemptState::Failed;
    failed.failure = Some("erp.rejected: SENTINEL-REASON".to_owned());
    store
        .commit(
            &id,
            version,
            CaseUpdate::new().with_attempt(failed).with_at(at(4)),
        )
        .await
        .unwrap();
    let mut waiting = Attempt::started(nid("n1"), 2, at(5), false);
    waiting.state = AttemptState::Waiting;
    store
        .commit(
            &id,
            version,
            CaseUpdate::new()
                .with_attempt(waiting)
                .with_timer(TimerRecord {
                    node: nid("n1"),
                    attempt: 2,
                    due: None,
                    event: None,
                    human: Some("approve".to_owned()),
                })
                .with_at(at(6)),
        )
        .await
        .unwrap();

    let records = store.list_after(NodeRunSeq(0), 100).await.unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].failure_code.as_deref(), Some("erp.rejected"));
    assert_eq!(records[2].wait, Some(LoggedWait::HumanTask));
    assert_eq!(records[2].human.as_deref(), Some("approve"));
    assert_eq!(
        records[0].at,
        at(2),
        "the commit instant, not the attempt's start"
    );
    let text: String = db
        .raw_client()
        .await
        .query_one(
            "SELECT string_agg(to_jsonb(t)::text, ' ') FROM spot_bpm_node_run_event t",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!text.contains("SENTINEL"), "{text}");
    assert!(!format!("{records:?}").contains("SENTINEL"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_without_an_instant_is_refused_before_any_write() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 4).await;
    let dag = chain(2);
    let record = store
        .create(new_case(&dag, "no_instant", "no-instant-key"))
        .await
        .unwrap();
    let id = CaseId::new("no_instant").unwrap();
    let update = CaseUpdate::new()
        .with_attempt(Attempt::started(nid("n0"), 1, at(1), false))
        .with_variables(json!({ "k": 1 }));
    assert!(store.commit(&id, record.run.version, update).await.is_err());
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM spot_bpm_node_outcome").await,
        0
    );
    assert_eq!(log_rows(&db, "default").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_keeps_every_log_row_and_adds_no_object() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "default", 4).await;
    fill(&store, "again_case", "again-key", 3).await;
    store
        .save_checkpoint(
            "insight",
            NodeRunSeq(0),
            Checkpoint {
                seq: NodeRunSeq(2),
                state: json!({ "n": 2 }),
            },
        )
        .await
        .unwrap();
    let before = dump_log(&db).await;
    assert!(!before.is_empty());
    let names = log_objects(&db).await;
    for _ in 0..3 {
        store.prepare().await.unwrap();
    }
    assert_eq!(dump_log(&db).await, before);
    assert_eq!(log_objects(&db).await, names);
    for expected in [
        "spot_bpm_node_run_consumer",
        "spot_bpm_node_run_event",
        "spot_bpm_node_run_event_pkey",
        "spot_bpm_node_run_event_replay_idx",
        "spot_bpm_node_run_seq",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} in {names:?}"
        );
    }
    // The log tables carry no foreign key: removing a Case never fights a prune.
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM pg_constraint WHERE contype = 'f' \
             AND conrelid::regclass::text LIKE 'spot_bpm_node_run_%'"
        )
        .await,
        0
    );
}
