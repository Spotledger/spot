//! Request effects and commit points hosted over a real PostgreSQL ledger (WP-BPM-15.T4;
//! ADR-80 section 6, ADR-45).
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset (the convention of `tests/process_documents.rs`). The record
//! file, the reversal journal, the hold ledger and the audit are the real tables; the parts that
//! need a live site (the lifecycle a reversal goes through, who may supervise, the business graph)
//! are the seams of [`Wiring`], filled with in-memory fakes. There is no HTTP route in this
//! module: its operations are called by the engine and by the routes of other modules, and the
//! refusal of an operation is its stable code, which these tests assert.
//!
//! What is covered: a Run that posted a document and holds an external call, cancelled: the
//! first effect is compensated once and the held call is discarded; an irreversible effect
//! stopping with `needs_attention` and a supervisor notice; a crash in the middle of compensation
//! resuming each reversal exactly once; receipts holding no payload, the log being append-only
//! and a retried write changing nothing; a person without the supervise permission being refused
//! (and the same answer for an unknown request); a permission source that cannot answer denying;
//! a graph that cannot answer denying the undo; commit points releasing in order and once; a site
//! with no dispatcher keeping its effects held; two-site isolation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use deadpool_postgres::Pool;
use serde_json::json;
use spot_app_api::prelude::{OffsetDateTime, RecordId, Version};
use spot_app_api::{
    ActionRef, BoxFuture, EffectClassName, EffectDecl, EffectKind, EffectReceipt, Hex32, HookPoint,
    OpaqueRef, Operation, OperationReceipt, Phase, ReceiptState, Reversal, StepName, StoreName,
    Target,
};
use spot_bpm_commit::{NodeAdmission, NodeEffects, RegionPlan};
use spot_bpm_effects::{
    GraphDependants, NoGraph, PortError, RecordRef, Refusal, ReversalCall, ReversalPort,
    StaticDeclarations,
};
use spot_cli::services::process_effects::{
    AttentionNotice, EffError, EffectNotice, EffectsHost, HoldParts, NoDispatcher, NoPartner,
    NoPlans, PlanSource, Recorded, Settings, Supervisors, UndoReport, Wiring,
};
use spot_core::{Clock, ManualClock, SiteId};
use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId};
use spot_effect_hold::{
    AuditSink, Dispatcher, EffectClass, EffectRef, LedgerStore, Notifier, PolicyTable,
    RecordingAudit, RecordingDispatcher, RecordingNotifier,
};
use spot_effect_hold_pg::PgLedger;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::DeclaredEffect;

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn rid(n: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[8..].copy_from_slice(&n.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::from_bytes(bytes).unwrap()
}

fn rec(n: u64) -> RecordRef {
    RecordRef::new("doc", rid(n).to_string())
}

fn person() -> String {
    format!("user:{}", RecordId::new_v7())
}

// ---------------------------------------------------------------------------------------------
// Receipts and declarations.
// ---------------------------------------------------------------------------------------------

fn effect(step: &str, class: &str, kind: EffectKind) -> EffectReceipt {
    EffectReceipt {
        step: StepName::new(step).unwrap(),
        class: EffectClassName::new(class).unwrap(),
        kind,
        store_refs: vec![OpaqueRef::new("row").unwrap()],
        count: 1,
        digest: Hex32::new(&"0".repeat(64)).unwrap(),
        state: ReceiptState::Applied,
    }
}

fn post_receipt(record: &RecordRef, wire: bool) -> OperationReceipt {
    let mut effects = vec![effect(
        "fake:post_ledger",
        "balance_movement",
        EffectKind::Internal,
    )];
    if wire {
        effects.push(effect("fake:wire", "bank_wire", EffectKind::External));
    }
    OperationReceipt {
        operation: Operation::Post,
        doctype: record.doctype.clone(),
        name: record.name.clone(),
        version: Version(2),
        effects,
        committed_at: start(),
    }
}

fn declared(step: &str, kind: EffectKind, class: &str, reversal: Reversal) -> DeclaredEffect {
    DeclaredEffect {
        step: StepName::new(step).unwrap(),
        app: "fake".to_owned(),
        point: HookPoint::of(Phase::After, Target::Op(Operation::Post)),
        decl: EffectDecl::new(
            kind,
            EffectClassName::new(class).unwrap(),
            vec![StoreName::new("ledger").unwrap()],
            reversal,
        ),
    }
}

fn declarations() -> StaticDeclarations {
    StaticDeclarations::new()
        .with(
            "doc",
            Operation::Post,
            declared(
                "fake:post_ledger",
                EffectKind::Internal,
                "balance_movement",
                Reversal::ReversibleBy(Operation::Cancel),
            ),
        )
        .with(
            "doc",
            Operation::Post,
            declared(
                "fake:wire",
                EffectKind::External,
                "bank_wire",
                Reversal::Irreversible {
                    reason: "money left".to_owned(),
                },
            ),
        )
        .with(
            "doc",
            Operation::Post,
            declared(
                "fake:notify",
                EffectKind::External,
                "mail",
                Reversal::CompensableBy(ActionRef::new("fake:send_correction").unwrap()),
            ),
        )
}

// ---------------------------------------------------------------------------------------------
// Fakes for the live-site seams.
// ---------------------------------------------------------------------------------------------

/// The lifecycle: counts what it really ran per key, and can be told to go down or to refuse.
#[derive(Default)]
struct FakeReversal {
    ran: Mutex<BTreeMap<String, u32>>,
    calls: AtomicUsize,
    down_after: Mutex<Option<usize>>,
    refuse: Mutex<BTreeSet<String>>,
}

impl FakeReversal {
    fn run(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self
            .down_after
            .lock()
            .unwrap()
            .is_some_and(|limit| n >= limit)
        {
            return Err(PortError::Unavailable("lifecycle_down".to_owned()));
        }
        if self.refuse.lock().unwrap().contains(&call.key) {
            return Err(PortError::Refused(Refusal {
                code: "dependants_outside_request".to_owned(),
                dependants: Vec::new(),
                alternatives: Vec::new(),
            }));
        }
        *self
            .ran
            .lock()
            .unwrap()
            .entry(call.key.clone())
            .or_default() += 1;
        Ok(OperationReceipt {
            operation: call.operation,
            doctype: call.record.doctype.clone(),
            name: call.record.name.clone(),
            version: Version(call.expected.0 + 1),
            effects: Vec::new(),
            committed_at: call.now,
        })
    }

    fn ran_total(&self) -> u32 {
        self.ran.lock().unwrap().values().sum()
    }

    fn most_runs_of_one_key(&self) -> u32 {
        self.ran
            .lock()
            .unwrap()
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }
}

impl ReversalPort for FakeReversal {
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.run(call)
    }
    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.run(call)
    }
}

/// Answers by actor; an actor it does not know is a source that cannot answer.
struct Staff(BTreeMap<String, bool>);

impl Supervisors for Staff {
    fn may_supervise<'a>(&'a self, actor: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move { self.0.get(actor).copied() })
    }
}

#[derive(Default)]
struct Notices(Mutex<Vec<AttentionNotice>>);

impl EffectNotice for Notices {
    fn needs_attention<'a>(&'a self, notice: &'a AttentionNotice) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.0.lock().unwrap().push(notice.clone()) })
    }
}

/// A graph that cannot answer.
struct DownGraph;

impl GraphDependants for DownGraph {
    fn dependants(&self, _record: &RecordRef) -> Result<Vec<RecordRef>, String> {
        Err("graph_down".to_owned())
    }
}

struct Plan(Arc<RegionPlan>);

impl PlanSource for Plan {
    fn plan(&self, _request: &str) -> Arc<RegionPlan> {
        Arc::clone(&self.0)
    }
}

/// `start -> a -> b -> c1 -> end`, `c1` the commit point; `a` sends mail, `b` calls a webhook.
fn held_plan() -> Arc<RegionPlan> {
    let kinds = [
        ("start", "trigger"),
        ("a", "document"),
        ("b", "document"),
        ("c1", "commit"),
        ("end", "document"),
    ];
    let nodes = kinds
        .iter()
        .map(|(n, k)| Node::new(node(n), *k, JoinRule::All, json!({})).unwrap())
        .collect();
    let edges = kinds
        .windows(2)
        .map(|w| Edge::new(node(w[0].0), node(w[1].0), None))
        .collect();
    let dag =
        CompiledDag::compile(&DagDefinition::new("proc", 1, nodes, edges), &Limits::new()).unwrap();
    let mut effects: NodeEffects = BTreeMap::new();
    effects.insert(node("a"), [EffectClass::Mail].into());
    effects.insert(node("b"), [EffectClass::Webhook].into());
    Arc::new(
        RegionPlan::analyze(&dag, &[node("c1")].into(), &effects, &PolicyTable::new()).unwrap(),
    )
}

// ---------------------------------------------------------------------------------------------
// The site under test.
// ---------------------------------------------------------------------------------------------

struct Env {
    _db: TestDb,
    pool: Pool,
    clock: Arc<ManualClock>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let pool = PgBackend::connect_unchecked(db.config()).unwrap().pool();
    Some(Env {
        _db: db,
        pool,
        clock: Arc::new(ManualClock::new(start())),
    })
}

/// The seams a test may change; everything else is the same in every test.
struct Rig {
    reversal: Arc<FakeReversal>,
    staff: BTreeMap<String, bool>,
    graph: Arc<dyn GraphDependants>,
    dispatcher: Arc<dyn Dispatcher>,
    plans: Arc<dyn PlanSource>,
    notices: Arc<Notices>,
}

impl Rig {
    fn new() -> Self {
        Self {
            reversal: Arc::new(FakeReversal::default()),
            staff: BTreeMap::new(),
            graph: Arc::new(NoGraph),
            dispatcher: Arc::new(RecordingDispatcher::new()),
            plans: Arc::new(NoPlans),
            notices: Arc::new(Notices::default()),
        }
    }
}

async fn host(env: &Env, site: &str, rig: &Rig) -> EffectsHost {
    let hold_ledger = PgLedger::new(env.pool.clone());
    hold_ledger.prepare().await.unwrap();
    let host = Wiring {
        site: site.to_owned(),
        clock: Arc::clone(&env.clock) as Arc<dyn Clock>,
        pool: env.pool.clone(),
        settings: Settings::default(),
        runtime: tokio::runtime::Handle::current(),
        declarations: Arc::new(declarations()),
        graph: Arc::clone(&rig.graph),
        reversal: Arc::clone(&rig.reversal) as Arc<dyn ReversalPort>,
        partner: Arc::new(NoPartner),
        supervisors: Arc::new(Staff(rig.staff.clone())),
        notice: Arc::clone(&rig.notices) as Arc<dyn EffectNotice>,
        hold: HoldParts {
            ledger: Arc::new(hold_ledger) as Arc<dyn LedgerStore>,
            dispatcher: Arc::clone(&rig.dispatcher),
            notifier: Arc::new(RecordingNotifier::new()) as Arc<dyn Notifier>,
            audit: Arc::new(RecordingAudit::new()) as Arc<dyn AuditSink>,
            plans: Arc::clone(&rig.plans),
        },
    }
    .build()
    .unwrap();
    host.prepare().await.unwrap();
    host
}

async fn record_post(
    host: &EffectsHost,
    env: &Env,
    request: &str,
    requester: &str,
    at_node: &str,
    n: u64,
    wire: bool,
) -> bool {
    host.record(Recorded {
        request,
        requester,
        node: at_node,
        attempt: 1,
        op_index: 0,
        receipt: post_receipt(&rec(n), wire),
        links: BTreeSet::new(),
        now: env.clock.now_utc(),
    })
    .await
    .unwrap()
}

async fn audit_rows(env: &Env, site: &str, rule: &str) -> i64 {
    let client = env.pool.get().await.unwrap();
    client
        .query_one(
            "SELECT count(*) FROM spot_bpm_eff_audit WHERE tenant = $1 AND rule = $2",
            &[&site, &rule],
        )
        .await
        .unwrap()
        .get(0)
}

fn held_effect(site: &str, class: EffectClass, key: &str) -> EffectRef {
    EffectRef::new(
        SiteId::new(site).unwrap(),
        class,
        "call",
        key,
        1_700_000_000_000_000,
    )
}

// ---------------------------------------------------------------------------------------------
// Undo and the hold.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_run_compensates_the_first_effect_once_and_discards_the_held_call() {
    let Some(env) = env().await else { return };
    let mut rig = Rig::new();
    rig.plans = Arc::new(Plan(held_plan()));
    let host = host(&env, "main", &rig).await;
    let alice = person();
    assert!(record_post(&host, &env, "run_1", &alice, "a", 1, false).await);
    // The webhook of the next node is held until the commit point.
    let admitted = host
        .hold()
        .admit(
            "run_1",
            &node("b"),
            held_effect("main", EffectClass::Webhook, "k1"),
        )
        .await
        .unwrap();
    assert!(matches!(admitted, NodeAdmission::Held(_)));
    assert_eq!(host.hold().pending("run_1").await.unwrap(), 1);

    let report = host
        .undo(&alice, "run_1", None, env.clock.now_utc())
        .await
        .unwrap();
    match report {
        UndoReport::Done {
            reversed,
            discarded,
            ..
        } => {
            assert_eq!(reversed, 1);
            assert_eq!(discarded, 1);
        }
        other @ UndoReport::NeedsAttention { .. } => panic!("expected done, got {other:?}"),
    }
    assert_eq!(rig.reversal.ran_total(), 1);
    assert_eq!(rig.reversal.most_runs_of_one_key(), 1);
    assert_eq!(host.hold().pending("run_1").await.unwrap(), 0);
    let state = host.state(&alice, "run_1").await.unwrap().unwrap();
    assert_eq!(state.state, "done");
    assert_eq!(audit_rows(&env, "main", "completed").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_irreversible_effect_stops_with_needs_attention_and_a_supervisor_notice() {
    let Some(env) = env().await else { return };
    let rig = Rig::new();
    let host = host(&env, "main", &rig).await;
    let alice = person();
    record_post(&host, &env, "run_1", &alice, "a", 1, true).await;
    let report = host
        .undo(&alice, "run_1", None, env.clock.now_utc())
        .await
        .unwrap();
    assert!(matches!(report, UndoReport::NeedsAttention { .. }));
    // Nothing ran: a point of no return blocks the whole plan before the first call.
    assert_eq!(rig.reversal.ran_total(), 0);
    let state = host.state(&alice, "run_1").await.unwrap().unwrap();
    assert_eq!(state.state, "needs_attention");
    assert_eq!(rig.notices.0.lock().unwrap().len(), 1);
    assert_eq!(rig.notices.0.lock().unwrap()[0].request, "run_1");
    // The state is stable: asking again stops at the same place and does not skip anything.
    let again = host
        .undo(&alice, "run_1", None, env.clock.now_utc())
        .await
        .unwrap();
    assert!(matches!(again, UndoReport::NeedsAttention { .. }));
    assert_eq!(rig.reversal.ran_total(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_reversal_is_a_decision_not_a_skip() {
    let Some(env) = env().await else { return };
    let rig = Rig::new();
    let host = host(&env, "main", &rig).await;
    let alice = person();
    record_post(&host, &env, "run_1", &alice, "a", 1, false).await;
    record_post(&host, &env, "run_1", &alice, "b", 2, false).await;
    // The lifecycle refuses the later record's reversal (a closed period, say).
    let key = "undo:run_1:1:0".to_owned();
    rig.reversal.refuse.lock().unwrap().insert(key);
    let report = host
        .undo(&alice, "run_1", None, env.clock.now_utc())
        .await
        .unwrap();
    assert!(matches!(report, UndoReport::NeedsAttention { .. }));
    assert_eq!(rig.reversal.ran_total(), 0);
    assert_eq!(rig.notices.0.lock().unwrap().len(), 1);
    assert_eq!(
        host.state(&alice, "run_1").await.unwrap().unwrap().state,
        "needs_attention"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_in_the_middle_of_compensation_resumes_each_reversal_once() {
    let Some(env) = env().await else { return };
    let rig = Rig::new();
    let first = host(&env, "main", &rig).await;
    let alice = person();
    record_post(&first, &env, "run_1", &alice, "a", 1, false).await;
    record_post(&first, &env, "run_1", &alice, "b", 2, false).await;
    // The lifecycle goes down after the first reversal.
    *rig.reversal.down_after.lock().unwrap() = Some(1);
    let outcome = first.undo(&alice, "run_1", None, env.clock.now_utc()).await;
    assert_eq!(outcome.unwrap_err(), EffError::UNAVAILABLE);
    assert_eq!(rig.reversal.ran_total(), 1);
    assert_eq!(
        first.state(&alice, "run_1").await.unwrap().unwrap().state,
        "interrupted"
    );
    // The node dies: its memory goes with it. A new host over the same database resumes.
    drop(first);
    *rig.reversal.down_after.lock().unwrap() = None;
    let second = host(&env, "main", &rig).await;
    assert_eq!(second.resume_pending().await, 1);
    assert_eq!(rig.reversal.ran_total(), 2);
    assert_eq!(rig.reversal.most_runs_of_one_key(), 1);
    assert_eq!(
        second.state(&alice, "run_1").await.unwrap().unwrap().state,
        "done"
    );
    // A finished undo is not resumed again.
    assert_eq!(second.resume_pending().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graph_that_cannot_answer_denies_the_undo() {
    let Some(env) = env().await else { return };
    let mut rig = Rig::new();
    rig.graph = Arc::new(DownGraph);
    let host = host(&env, "main", &rig).await;
    let alice = person();
    record_post(&host, &env, "run_1", &alice, "a", 1, false).await;
    let outcome = host.undo(&alice, "run_1", None, env.clock.now_utc()).await;
    assert_eq!(outcome.unwrap_err().code(), "bpm_effects.graph_unavailable");
    assert_eq!(rig.reversal.ran_total(), 0);
}

// ---------------------------------------------------------------------------------------------
// Who may undo, and the record file.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_requester_or_a_supervisor_may_undo_a_request() {
    let Some(env) = env().await else { return };
    let (alice, bob, carol, dave) = (person(), person(), person(), person());
    let mut rig = Rig::new();
    rig.staff.insert(bob.clone(), false);
    rig.staff.insert(carol.clone(), true);
    // `dave` is not in the table: the permission source cannot answer.
    let host = host(&env, "main", &rig).await;
    record_post(&host, &env, "run_1", &alice, "a", 1, false).await;

    for denied in [&bob, &dave] {
        let outcome = host.undo(denied, "run_1", None, env.clock.now_utc()).await;
        assert_eq!(outcome.unwrap_err(), EffError::REQUEST_UNKNOWN);
        assert!(host.state(denied, "run_1").await.is_err());
    }
    // The same answer for a request nobody started.
    let unknown = host.undo(&bob, "run_404", None, env.clock.now_utc()).await;
    assert_eq!(unknown.unwrap_err(), EffError::REQUEST_UNKNOWN);
    assert_eq!(audit_rows(&env, "main", "not_supervisor").await, 4);
    assert_eq!(audit_rows(&env, "main", "request_unknown").await, 1);
    assert_eq!(rig.reversal.ran_total(), 0);

    // The supervisor may.
    let report = host
        .undo(&carol, "run_1", None, env.clock.now_utc())
        .await
        .unwrap();
    assert!(matches!(report, UndoReport::Done { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipts_hold_no_payload_and_the_log_is_append_only() {
    let Some(env) = env().await else { return };
    let host = host(&env, "main", &Rig::new()).await;
    let alice = person();
    assert!(record_post(&host, &env, "run_1", &alice, "a", 1, false).await);
    // A retried write changes nothing.
    assert!(!record_post(&host, &env, "run_1", &alice, "a", 1, false).await);

    let client = env.pool.get().await.unwrap();
    let rows = client
        .query(
            "SELECT body, links FROM spot_bpm_eff_log WHERE tenant = 'main' AND request = 'run_1'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let body: String = rows[0].get(0);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let keys: BTreeSet<&str> = parsed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "operation",
            "doctype",
            "name",
            "version",
            "effects",
            "committed_at"
        ])
    );
    for effect in parsed["effects"].as_array().unwrap() {
        let keys: BTreeSet<&str> = effect
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "step",
                "class",
                "kind",
                "store_refs",
                "count",
                "digest",
                "state"
            ])
        );
    }
    // Append-only: the database itself refuses a change or a removal.
    let update = client
        .execute(
            "UPDATE spot_bpm_eff_log SET body = 'x' WHERE tenant = 'main'",
            &[],
        )
        .await;
    assert!(update.is_err());
    let delete = client
        .execute("DELETE FROM spot_bpm_eff_log WHERE tenant = 'main'", &[])
        .await;
    assert!(delete.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_share_nothing() {
    let Some(env) = env().await else { return };
    let rig = Rig::new();
    let a = host(&env, "main", &rig).await;
    let b = host(&env, "site-b", &rig).await;
    let alice = person();
    record_post(&a, &env, "run_1", &alice, "a", 1, false).await;
    // Site B does not know the request, whoever asks.
    let outcome = b.undo(&alice, "run_1", None, env.clock.now_utc()).await;
    assert_eq!(outcome.unwrap_err(), EffError::REQUEST_UNKNOWN);
    assert!(b.state(&alice, "run_1").await.is_err());
    assert_eq!(rig.reversal.ran_total(), 0);
    // The same request id on site B is its own request with its own file and requester.
    let bob = person();
    record_post(&b, &env, "run_1", &bob, "a", 2, false).await;
    let report = b
        .undo(&bob, "run_1", None, env.clock.now_utc())
        .await
        .unwrap();
    assert!(matches!(report, UndoReport::Done { reversed: 1, .. }));
    assert_eq!(rig.reversal.ran_total(), 1);
    // Site A's request is untouched.
    assert!(a.state(&alice, "run_1").await.unwrap().is_none());
}

// ---------------------------------------------------------------------------------------------
// Commit points.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_point_releases_in_order_and_a_repeat_sends_nothing_twice() {
    let Some(env) = env().await else { return };
    let mut rig = Rig::new();
    let dispatcher = Arc::new(RecordingDispatcher::new());
    rig.dispatcher = Arc::clone(&dispatcher) as Arc<dyn Dispatcher>;
    rig.plans = Arc::new(Plan(held_plan()));
    let host = host(&env, "main", &rig).await;
    let now = 1_700_000_000_000_000;
    for (at, class, key) in [
        ("a", EffectClass::Mail, "k-mail"),
        ("b", EffectClass::Webhook, "k-hook"),
    ] {
        let admitted = host
            .hold()
            .admit("run_1", &node(at), held_effect("main", class, key))
            .await
            .unwrap();
        assert!(matches!(admitted, NodeAdmission::Held(_)));
    }
    // Nothing leaves before the commit point.
    assert!(dispatcher.calls().is_empty());
    let outcome = host.hold().commit("run_1", &node("c1"), now).await.unwrap();
    assert!(outcome.completed);
    let sent: Vec<String> = dispatcher.calls().iter().map(|c| c.key.clone()).collect();
    assert_eq!(sent, vec!["k-mail".to_owned(), "k-hook".to_owned()]);
    // A repeat after a crash dispatches only what is not yet out.
    host.hold().commit("run_1", &node("c1"), now).await.unwrap();
    assert_eq!(dispatcher.unique_count(), 2);
    assert_eq!(host.hold().pending("run_1").await.unwrap(), 0);
    // A node that is not a commit point is refused.
    assert!(host.hold().commit("run_1", &node("a"), now).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_no_dispatcher_keeps_its_effects_held() {
    let Some(env) = env().await else { return };
    let mut rig = Rig::new();
    rig.dispatcher = Arc::new(NoDispatcher);
    rig.plans = Arc::new(Plan(held_plan()));
    let host = host(&env, "main", &rig).await;
    host.hold()
        .admit(
            "run_1",
            &node("a"),
            held_effect("main", EffectClass::Mail, "k-mail"),
        )
        .await
        .unwrap();
    let outcome = host
        .hold()
        .commit("run_1", &node("c1"), 1_700_000_000_000_000)
        .await;
    assert_eq!(outcome.unwrap_err().code(), "effect_hold.dispatch_failed");
    assert_eq!(host.hold().pending("run_1").await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_commit_point_nothing_is_held() {
    let Some(env) = env().await else { return };
    let host = host(&env, "main", &Rig::new()).await;
    let admitted = host
        .hold()
        .admit(
            "run_1",
            &node("a"),
            held_effect("main", EffectClass::Mail, "k"),
        )
        .await
        .unwrap();
    assert_eq!(admitted, NodeAdmission::Flows);
    assert_eq!(host.hold().pending("run_1").await.unwrap(), 0);
}
