//! The node-run transition log through the real engine: exact ordered records with exact
//! instants for a full run, a retried integration, a cancel, the exactly-once and rollback
//! guarantees, the checkpoint compare-and-set, paging and tenant isolation.
//!
//! Every in-memory future is immediately ready, so a no-op waker is all the executor needs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_bpm_engine::{
    Attempt, AttemptState, CaseId, CaseStore, CaseUpdate, Checkpoint, EffectLedger, Engine,
    FlakyHandler, HandlerRegistry, Harness, InMemoryCaseStore, InMemoryNodeRunLogFactory,
    InMemoryTimerPort, LoggedState, LoggedWait, NodeCtx, NodeHandler, NodeKind, NodeOutcome,
    NodeRunLog, NodeRunRecord, NodeRunSeq, TimerRecord, WaitSpec, node_run_log_contract,
};
use spot_bpm_model::{ProcessVersion, from_text};
use spot_dag::NodeId;
use spot_dag_exec::Instant;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

const HUMAN: &str = r#"
name = "human"
app = "log_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "approve"
kind = "decision"
title = "Approve?"
assignee = "user:alice"
outcomes = ["approve", "reject"]

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "approve"

[[edges]]
from = "approve"
to = "done"
"#;

const RETRY: &str = r#"
name = "retry"
app = "log_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "call"
kind = "integration"
connector = "http"
action = "post"
retry_attempts = 3
retry_delay_secs = 1

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "call"

[[edges]]
from = "call"
to = "done"
"#;

fn version(text: &str) -> ProcessVersion {
    from_text(text).expect("the process text must parse")
}

fn node(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// A decision handler that waits for a person under the key `approve-task`.
struct HumanWait;

impl NodeHandler for HumanWait {
    fn kind(&self) -> NodeKind {
        NodeKind::Decision
    }

    fn run(&self, _ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        NodeOutcome::Wait(WaitSpec::HumanTask {
            key: "approve-task".to_owned(),
            due: None,
        })
    }
}

fn secs(record: &NodeRunRecord) -> i64 {
    record.at.seconds
}

fn shape(records: &[NodeRunRecord]) -> Vec<(String, u32, LoggedState, i64)> {
    records
        .iter()
        .map(|r| (r.node.to_string(), r.attempt, r.state, secs(r)))
        .collect()
}

async fn all(log: &dyn NodeRunLog) -> Vec<NodeRunRecord> {
    log.list_after(NodeRunSeq(0), 1000).await.unwrap()
}

#[test]
fn the_in_memory_store_passes_the_node_run_log_contract() {
    block_on(async {
        node_run_log_contract(&InMemoryNodeRunLogFactory::new())
            .await
            .unwrap();
    });
}

#[test]
fn a_full_run_with_a_human_wait_logs_the_exact_ordered_records() {
    block_on(async {
        let ledger = Arc::new(EffectLedger::new());
        let mut registry = HandlerRegistry::logic_default();
        registry.register(HumanWait);
        let harness = Harness::with_ledger(registry, ledger);
        let id = CaseId::new("case_human").unwrap();
        harness
            .start(&version(HUMAN), id.clone(), "human")
            .await
            .unwrap();
        let t0 = harness.clock().now().unix_timestamp();
        for _ in 0..6 {
            if !harness.step(&id).await.unwrap().made_progress() {
                break;
            }
        }
        harness.clock().advance(600);
        harness
            .engine()
            .signal(&id, &node("approve"), None, harness.clock().now())
            .await
            .unwrap();
        harness.drive(&id, 20).await.unwrap();

        let log = harness.engine().store();
        let records = all(log).await;
        let waiting = records
            .iter()
            .find(|r| r.node == node("approve") && r.state == LoggedState::Waiting)
            .expect("the human wait is logged");
        assert_eq!(waiting.wait, Some(LoggedWait::HumanTask));
        assert_eq!(waiting.human.as_deref(), Some("approve-task"));
        let approve: Vec<_> = shape(&records)
            .into_iter()
            .filter(|(n, ..)| n == "approve")
            .collect();
        assert_eq!(
            approve,
            vec![
                ("approve".to_owned(), 1, LoggedState::Started, t0),
                ("approve".to_owned(), 1, LoggedState::Waiting, t0),
                ("approve".to_owned(), 1, LoggedState::Done, t0 + 600),
            ]
        );
        assert!(records.windows(2).all(|w| w[0].seq < w[1].seq));
        // The trigger and the terminal join were logged too, each started then done.
        assert!(
            records
                .iter()
                .any(|r| r.node == node("done") && r.state == LoggedState::Done)
        );
    });
}

#[test]
fn a_retried_integration_logs_started_waiting_started_done() {
    block_on(async {
        let ledger = Arc::new(EffectLedger::new());
        let mut registry = HandlerRegistry::logic_default();
        registry.register(FlakyHandler::new(NodeKind::Integration, ledger.clone(), 1));
        let harness = Harness::with_ledger(registry, ledger);
        let id = CaseId::new("case_retry_log").unwrap();
        harness
            .start(&version(RETRY), id.clone(), "retry")
            .await
            .unwrap();
        harness.drive(&id, 100).await.unwrap();
        let records = all(harness.engine().store()).await;
        let call: Vec<_> = records.iter().filter(|r| r.node == node("call")).collect();
        let states: Vec<_> = call.iter().map(|r| (r.attempt, r.state, r.wait)).collect();
        assert_eq!(
            states,
            vec![
                (1, LoggedState::Started, None),
                (1, LoggedState::Waiting, Some(LoggedWait::IntegrationRetry)),
                (2, LoggedState::Started, None),
                (2, LoggedState::Done, None),
            ]
        );
        assert!(call[1].is_retry);
        assert!(call[1].at <= call[2].at);
        // The failed attempt's plain-language reason never reaches the log.
        assert!(!format!("{records:?}").contains("scripted_failure"));
        assert!(!format!("{records:?}").contains("attempt 1"));
    });
}

#[test]
fn a_terminal_failure_logs_its_stable_code_only() {
    block_on(async {
        let ledger = Arc::new(EffectLedger::new());
        let mut registry = HandlerRegistry::logic_default();
        registry.register(FlakyHandler::new(NodeKind::Integration, ledger.clone(), 9));
        let harness = Harness::with_ledger(registry, ledger);
        let id = CaseId::new("case_fail_log").unwrap();
        harness
            .start(&version(RETRY), id.clone(), "fail")
            .await
            .unwrap();
        harness.drive(&id, 100).await.unwrap();
        let records = all(harness.engine().store()).await;
        let failed: Vec<_> = records
            .iter()
            .filter(|r| r.node == node("call") && r.state == LoggedState::Failed)
            .collect();
        assert_eq!(failed.len(), 1, "one terminal failure per attempt");
        assert_eq!(failed[0].failure_code.as_deref(), Some("scripted_failure"));
        assert!(!format!("{records:?}").contains("attempt 3"));
    });
}

#[test]
fn a_cancelled_case_logs_cancelled_for_its_open_node() {
    block_on(async {
        let ledger = Arc::new(EffectLedger::new());
        let mut registry = HandlerRegistry::logic_default();
        registry.register(HumanWait);
        let harness = Harness::with_ledger(registry, ledger);
        let id = CaseId::new("case_cancel_log").unwrap();
        harness
            .start(&version(HUMAN), id.clone(), "cancel")
            .await
            .unwrap();
        for _ in 0..4 {
            harness.step(&id).await.unwrap();
        }
        harness.clock().advance(30);
        harness
            .engine()
            .cancel(&id, harness.clock().now())
            .await
            .unwrap();
        let records = all(harness.engine().store()).await;
        let cancelled: Vec<_> = records
            .iter()
            .filter(|r| r.state == LoggedState::Cancelled)
            .collect();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].node, node("approve"));
        assert_eq!(cancelled[0].attempt, 1);
        assert_eq!(
            cancelled[0].at.seconds,
            harness.clock().now().unix_timestamp()
        );
    });
}

#[test]
fn an_update_without_an_instant_is_refused_and_logs_nothing() {
    block_on(async {
        let harness = Harness::default();
        let id = CaseId::new("case_no_at").unwrap();
        let record = harness
            .start(&version(RETRY), id.clone(), "no-at")
            .await
            .unwrap();
        let store = harness.engine().store();
        let attempt = Attempt::started(
            node("start"),
            1,
            Instant {
                seconds: 1,
                nanos: 0,
            },
            false,
        );
        let error = store
            .commit(
                &id,
                record.run.version,
                CaseUpdate::new().with_attempt(attempt),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "bpm_engine.store_failed");
        let timer = TimerRecord {
            node: node("start"),
            attempt: 1,
            due: Some(Instant {
                seconds: 5,
                nanos: 0,
            }),
            event: None,
            human: None,
        };
        assert!(
            store
                .commit(&id, record.run.version, CaseUpdate::new().with_timer(timer))
                .await
                .is_err()
        );
        assert!(all(store).await.is_empty());
        let loaded = store.load(&id).await.unwrap().unwrap();
        assert!(
            loaded.attempts.is_empty(),
            "a refused commit changed nothing"
        );
    });
}

#[test]
fn a_version_conflict_and_a_replay_add_nothing() {
    block_on(async {
        let store = InMemoryCaseStore::new();
        let engine = Engine::new(
            store,
            InMemoryTimerPort::new(),
            HandlerRegistry::logic_default(),
        );
        let id = CaseId::new("case_conflict").unwrap();
        let now = spot_bpm_engine::harness::default_now();
        let record = engine
            .start(&version(RETRY), id.clone(), "c".to_owned(), None, now)
            .await
            .unwrap();
        let store = engine.store();
        let at = Instant::from_offset(now);
        let started = Attempt::started(node("start"), 2, at, false);
        store
            .commit(
                &id,
                record.run.version,
                CaseUpdate::new().with_attempt(started.clone()).with_at(at),
            )
            .await
            .unwrap();
        let count = all(store).await.len();
        assert_eq!(count, 1);
        // A stale version rolls back whole.
        let mut other = started.clone();
        other.attempt = 3;
        let conflict = store
            .commit(
                &id,
                record.run.version + 7,
                CaseUpdate::new().with_attempt(other).with_at(at),
            )
            .await;
        assert!(conflict.is_err());
        // An earlier attempt number never overwrites a later one, and logs nothing.
        let mut earlier = started.clone();
        earlier.attempt = 1;
        earlier.state = AttemptState::Done;
        store
            .commit(
                &id,
                record.run.version,
                CaseUpdate::new().with_attempt(earlier).with_at(at),
            )
            .await
            .unwrap();
        // The same attempt again is a replay.
        store
            .commit(
                &id,
                record.run.version,
                CaseUpdate::new().with_attempt(started).with_at(at),
            )
            .await
            .unwrap();
        assert_eq!(all(store).await.len(), count);
    });
}

#[test]
fn the_checkpoint_is_compare_and_set() {
    block_on(async {
        let store = InMemoryCaseStore::with_tenant("t");
        let next = |seq| Checkpoint {
            seq: NodeRunSeq(seq),
            state: serde_json::json!({ "seen": seq }),
        };
        assert!(
            store
                .save_checkpoint("c1", NodeRunSeq(0), next(5))
                .await
                .unwrap()
        );
        assert!(
            !store
                .save_checkpoint("c1", NodeRunSeq(0), next(9))
                .await
                .unwrap()
        );
        assert!(
            store
                .save_checkpoint("c1", NodeRunSeq(5), next(9))
                .await
                .unwrap()
        );
        assert_eq!(store.load_checkpoint("c1").await.unwrap(), Some(next(9)));
        // A checkpoint cannot move backwards, and a bad name is refused.
        assert!(
            store
                .save_checkpoint("c1", NodeRunSeq(9), next(2))
                .await
                .is_err()
        );
        assert!(
            store
                .save_checkpoint("C1", NodeRunSeq(0), next(1))
                .await
                .is_err()
        );
        assert!(store.load_checkpoint("").await.is_err());
        // Another consumer has its own checkpoint.
        assert!(store.load_checkpoint("c2").await.unwrap().is_none());
    });
}

#[test]
fn two_tenants_never_see_each_others_records() {
    block_on(async {
        // Two independent stores built for two tenants, each driven by its own engine.
        let engine_a = Engine::new(
            InMemoryCaseStore::with_tenant("tenant_a"),
            InMemoryTimerPort::new(),
            HandlerRegistry::logic_default(),
        );
        let engine_b = Engine::new(
            InMemoryCaseStore::with_tenant("tenant_b"),
            InMemoryTimerPort::new(),
            HandlerRegistry::logic_default(),
        );
        let now = spot_bpm_engine::harness::default_now();
        let id_a = CaseId::new("case_a").unwrap();
        engine_a
            .start(&version(RETRY), id_a.clone(), "a".to_owned(), None, now)
            .await
            .unwrap();
        engine_a.step(&id_a, now).await.unwrap();
        let a = all(engine_a.store()).await;
        let b = all(engine_b.store()).await;
        assert!(!a.is_empty());
        assert!(b.is_empty());
        // One backend, two views: still only the owner's records.
        let shared = InMemoryCaseStore::with_tenant("tenant_a");
        let view = shared.view_for("tenant_b");
        assert!(all(&view).await.is_empty());
        assert_eq!(view.prune_before(NodeRunSeq(100)).await.unwrap(), 0);
    });
}

#[test]
fn prune_deletes_below_the_cut_for_this_tenant_only() {
    block_on(async {
        let engine = Engine::new(
            InMemoryCaseStore::with_tenant("t"),
            InMemoryTimerPort::new(),
            HandlerRegistry::logic_default(),
        );
        let now = spot_bpm_engine::harness::default_now();
        let id = CaseId::new("case_prune").unwrap();
        engine
            .start(&version(RETRY), id.clone(), "p".to_owned(), None, now)
            .await
            .unwrap();
        engine.step(&id, now).await.unwrap();
        let store = engine.store();
        let before = all(store).await;
        assert!(before.len() >= 2);
        let cut = before[1].seq;
        assert_eq!(store.prune_before(cut).await.unwrap(), 1);
        let after = all(store).await;
        assert_eq!(after.first().map(|r| r.seq), Some(cut));
        // Paging past the pruned records is unchanged.
        let page = store.list_after(before[0].seq, 1000).await.unwrap();
        assert_eq!(page, after);
    });
}
