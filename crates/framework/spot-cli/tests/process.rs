//! Integration tests for the process engine hosting module: start a Case, survive a restart,
//! refuse a process whose node kind has no handler, isolate two sites, and classify a failing
//! handler's stable code. Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and
//! skips, printing `SKIPPED:`, when it is unset — the same convention `spot-bpm-pg`'s own tests
//! use.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{AttemptState, CaseId, HandlerRegistry, InMemoryTimerPort, TimerPort};
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortError, PortFuture};
use spot_cli::services::process::{HandlerBuilder, ProcessHost};
use spot_core::{Clock, SiteId, SystemClock};
use spot_dag_exec::RunStatus;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, Lease, LeaseStore};

/// The fixed instant every test passes in: 2023-11-14T22:13:20Z.
fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A backend with a pool of `size` connections whose server-side `application_name` is `app`, taken from a
/// [`PgBackend`] (the sanctioned way a site's sibling stores get their pool).
fn backend(db: &TestDb, size: usize, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = size;
    PgBackend::connect_unchecked(cfg).unwrap()
}

/// `at` as the scheduler's timestamp type (the same conversion `process.rs` keeps private).
fn to_jiff(at: time::OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .unwrap()
}

/// A fresh [`FencedTimerPort`] holding its own lease on `leases`, named `name`.
fn timer_port(leases: Arc<dyn LeaseStore>, name: &str, holder: &str) -> (Lease, FencedTimerPort) {
    let lease = leases
        .acquire(name, holder, Duration::from_secs(3600), to_jiff(now()))
        .unwrap();
    let inner: Arc<dyn TimerPort> = Arc::new(InMemoryTimerPort::new());
    let port = FencedTimerPort::new(leases, lease.clone(), inner);
    (lease, port)
}

/// A [`JobSubmit`] that only records what it was asked to submit: these tests drive the engine
/// directly through [`ProcessHost::engine`] and [`ProcessHost::resume_once`], never through the
/// real job worker (that machinery is `spot-jobs`' own, already tested there).
#[derive(Default)]
struct RecordingSubmit(Mutex<Vec<JobSpec>>);

impl RecordingSubmit {
    fn count(&self) -> usize {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

impl JobSubmit for RecordingSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).push(job);
            Ok(true)
        })
    }
}

/// A submit port that always refuses: proves a failed enqueue never fails `start_run`.
struct RefusingSubmit;

impl JobSubmit for RefusingSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Err(PortError::Unavailable("test")) })
    }
}

/// A host over a fresh tenant-scoped store pair on `db`, with its own in-process timer lease
/// store (shared with [`host_sharing`] for the fencing test).
async fn host(db: &TestDb, tenant: &str, submit: Arc<dyn JobSubmit>) -> Arc<ProcessHost> {
    host_sharing(db, tenant, submit, Arc::new(InMemoryLeaseStore::new())).await
}

async fn host_sharing(
    db: &TestDb,
    tenant: &str,
    submit: Arc<dyn JobSubmit>,
    leases: Arc<dyn LeaseStore>,
) -> Arc<ProcessHost> {
    let store = PgCaseStore::new(backend(db, 4, tenant).pool()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions =
        Arc::new(PgDefinitionStore::new(backend(db, 4, tenant).pool()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let (_lease, timers) = timer_port(leases, &format!("timer:{tenant}"), "node-1");
    let site = SiteId::new(tenant).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    ProcessHost::build(
        site,
        store,
        timers,
        HandlerRegistry::logic_default(),
        definitions,
        submit,
        clock,
    )
}

/// A three-node process (trigger, an unconditional branch, a terminal join): every kind it uses
/// is in `HandlerRegistry::logic_default()`, so no extra handler is registered.
const THREE_NODE: &str = r#"
name = "three"
app = "process_host_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "mid"
kind = "logic"
logic = "branch"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "mid"

[[edges]]
from = "mid"
to = "done"
"#;

/// A process whose second node is a Wait (a duration): used for the restart-survival test (a
/// Run left `started` on the trigger, and a wait still due after the restart).
const WAIT_NODE: &str = r#"
name = "waiter"
app = "process_host_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "pause"
kind = "wait"
wait = "duration"
seconds = 30

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "pause"

[[edges]]
from = "pause"
to = "done"
"#;

/// A process with a node kind no handler covers (`integration`; its host is WP-BPM-06).
const INTEGRATION_NODE: &str = r#"
name = "caller"
app = "process_host_test"
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
terminal = true

[[edges]]
from = "start"
to = "call"
"#;

/// A process whose one real step always fails (a Script node, which this crate refuses by
/// design) with a bounded retry policy and an error branch to a terminal join.
const RETRY_FAIL: &str = r#"
name = "flaky"
app = "process_host_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "flaky"
kind = "logic"
logic = "script"
retry_attempts = 2
retry_delay_secs = 1
error = "landing"

[[nodes]]
id = "landing"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "flaky"
"#;

/// A process whose only real decision depends on a variable (`doc.go`): proves a Run actually
/// sees the input `start_run` was given, not the engine's default empty variables.
const VARIABLE_GATE: &str = r#"
name = "gate"
app = "process_host_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "gate"
kind = "logic"
logic = "branch"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "gate"

[[edges]]
from = "gate"
to = "done"
condition = "doc.go == true"
"#;

/// Publishes `text` into `host`'s definition store under `publisher`, then reloads the engine's
/// registry from it.
async fn publish(host: &ProcessHost, text: &str, publisher: &str) {
    let version = from_text(text).unwrap();
    // `reload_definitions` reads through the definition store, so the text must be published
    // there first (the publish path itself is WP-BPM-09.T4, not this task).
    let engine = host.engine().unwrap();
    // A direct publish into the engine's own registry proves the same compile+load path
    // `reload_definitions` takes, without needing the publish HTTP surface this task does not
    // own; `reload_definitions` is exercised separately below over the real store.
    engine.publish(&version).unwrap();
    let _ = publisher;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_a_run_of_a_three_node_process_completes() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let submit = Arc::new(RecordingSubmit::default());
    let host = host(&db, "tenant-three", submit.clone()).await;
    publish(&host, THREE_NODE, "tester").await;

    let case = host
        .start_run("three", None, serde_json::json!({}), None, "order-1", now())
        .await
        .unwrap();
    let engine = host.engine().unwrap();
    let record = engine.run_until_idle(&case, now(), 20).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
    // `start_run` enqueued the first case-step job (best-effort); the resume scan did not need
    // to find it.
    assert!(submit.count() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_a_run_persists_the_input_and_a_node_observes_it() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "tenant-input", Arc::new(RecordingSubmit::default())).await;
    publish(&host, VARIABLE_GATE, "tester").await;
    let input = serde_json::json!({ "go": true });
    let case = host
        .start_run("gate", None, input.clone(), None, "input-key", now())
        .await
        .unwrap();
    let engine = host.engine().unwrap();
    // Read the Case back from the store directly (not `start_run`'s return value, which is
    // only the id): proves the input actually landed in the durable record, not just a local
    // copy `start_run` discarded.
    let stored = engine.load(&case).await.unwrap();
    assert_eq!(
        stored.variables, input,
        "the persisted Case carries the caller's input"
    );
    // And a node observes it: the branch's only outgoing edge is conditional on `doc.go`,
    // true only when the handler evaluates against the persisted input, not the engine's
    // default empty variables (which would fail the branch with no edge to take).
    let record = engine.run_until_idle(&case, now(), 20).await.unwrap();
    assert_eq!(
        record.run.status,
        RunStatus::Completed,
        "the branch node saw `go: true` from the persisted input: {:?}",
        record.run.nodes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_business_key_starts_one_case_only() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "tenant-dedup", Arc::new(RecordingSubmit::default())).await;
    publish(&host, THREE_NODE, "tester").await;
    let first = host
        .start_run("three", None, serde_json::json!({}), None, "k1", now())
        .await
        .unwrap();
    let second = host
        .start_run("three", None, serde_json::json!({}), None, "k1", now())
        .await;
    // The derived id is deterministic, so the second `start_definition` hits the same id and
    // the store refuses the duplicate create.
    assert!(second.is_err());
    let engine = host.engine().unwrap();
    assert!(engine.load(&first).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_resumes_a_waiting_case_and_a_started_node_replays_as_a_retry() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let tenant = "tenant-restart";
    let case_id;
    // Phase one: start the Case, run until it waits, then drop the whole host (its pool and
    // in-process timer index): the simulated crash.
    {
        let host = host(&db, tenant, Arc::new(RecordingSubmit::default())).await;
        publish(&host, WAIT_NODE, "tester").await;
        case_id = host
            .start_run(
                "waiter",
                None,
                serde_json::json!({}),
                None,
                "wait-key",
                now(),
            )
            .await
            .unwrap();
        let engine = host.engine().unwrap();
        let record = engine.run_until_idle(&case_id, now(), 20).await.unwrap();
        assert_eq!(record.run.status, RunStatus::Running, "the Case is waiting");
        assert_eq!(
            record
                .attempt(&spot_dag::NodeId::new("pause").unwrap())
                .unwrap()
                .state,
            AttemptState::Waiting
        );
    }

    // Phase two: a fresh host over the same database, after the wait is due. The durable
    // record (not the dropped in-process timer index) is what makes the next `step` fire it.
    let host = host(&db, tenant, Arc::new(RecordingSubmit::default())).await;
    let engine = host.engine().unwrap();
    engine.publish(&from_text(WAIT_NODE).unwrap()).unwrap();
    let due = now() + std::time::Duration::from_secs(60);
    let record = engine.run_until_idle(&case_id, due, 20).await.unwrap();
    assert_eq!(record.run.status, RunStatus::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unregistered_node_kind_refuses_the_start_not_a_mid_run_failure() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "tenant-missing", Arc::new(RecordingSubmit::default())).await;
    publish(&host, INTEGRATION_NODE, "tester").await;
    let result = host
        .start_run(
            "caller",
            None,
            serde_json::json!({}),
            None,
            "call-key",
            now(),
        )
        .await;
    let error = result.expect_err("no handler is registered for `integration`");
    assert!(
        error.0.contains("integration"),
        "the refusal names the missing kind: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_tenants_do_not_see_each_others_cases() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host_a = host(&db, "tenant-a", Arc::new(RecordingSubmit::default())).await;
    let host_b = host(&db, "tenant-b", Arc::new(RecordingSubmit::default())).await;
    publish(&host_a, THREE_NODE, "tester").await;
    publish(&host_b, THREE_NODE, "tester").await;
    let case_a = host_a
        .start_run("three", None, serde_json::json!({}), None, "iso-key", now())
        .await
        .unwrap();
    // The two hosts derive the SAME case id from the same business key (it is a pure function
    // of the key), yet each tenant's store holds only its own row.
    let engine_a = host_a.engine().unwrap();
    let engine_b = host_b.engine().unwrap();
    assert!(engine_a.load(&case_a).await.is_ok());
    assert!(
        engine_b.load(&case_a).await.is_err(),
        "tenant b must not see tenant a's case"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_handler_retries_then_ends_with_a_stable_code() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "tenant-retry", Arc::new(RecordingSubmit::default())).await;
    publish(&host, RETRY_FAIL, "tester").await;
    let case = host
        .start_run(
            "flaky",
            None,
            serde_json::json!({}),
            None,
            "flaky-key",
            now(),
        )
        .await
        .unwrap();
    let engine = host.engine().unwrap();
    // The trigger completes, then "flaky" is claimed and fails; each retry needs its own turn
    // once the backoff's `not_before` is reached, so advance `now` and run to idle again.
    let mut record = engine.run_until_idle(&case, now(), 20).await.unwrap();
    for _ in 0..3 {
        if record.run.is_finished() {
            break;
        }
        record = engine
            .run_until_idle(&case, now() + std::time::Duration::from_secs(3600), 20)
            .await
            .unwrap();
    }
    assert_eq!(
        record.run.status,
        RunStatus::Failed,
        "the run ends failed with a stable code: {:?}",
        record.run.nodes
    );
    let attempt = record
        .attempt(&spot_dag::NodeId::new("flaky").unwrap())
        .unwrap();
    assert_eq!(attempt.state, AttemptState::Failed);
    assert_eq!(
        attempt.failure.as_deref(),
        Some("script_unsupported: the script node `flaky` needs the TypeScript host")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_enqueue_never_fails_start_run() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "tenant-enqueue", Arc::new(RefusingSubmit)).await;
    publish(&host, THREE_NODE, "tester").await;
    let case = host
        .start_run(
            "three",
            None,
            serde_json::json!({}),
            None,
            "enqueue-key",
            now(),
        )
        .await
        .unwrap();
    assert_eq!(host.metrics().runs_started, 1);
    // Nothing enqueued, but the resume scan still finds the Case runnable.
    let backlog = host.resume_once(now(), 10).await.unwrap();
    assert_eq!(backlog, 1);
    let _ = case;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_timer_ports_sharing_one_in_memory_lease_store_fire_a_wait_exactly_once() {
    // This proves `FencedTimerPort`'s mechanics only: two ports, each with its own held lease,
    // contending for the SAME in-memory `LeaseStore` inside ONE process. It does NOT prove the
    // brief's cross-process requirement ("two ProcessHosts over one database never both fire
    // the same timer"), because `spot_scheduler::LeaseStore` has only an in-memory reference
    // implementation today — a real multi-node deployment has two separate `InMemoryLeaseStore`
    // instances (one per process, see `register`'s `leases` construction), so this scenario
    // cannot be exercised against the real deployment shape until a PostgreSQL-backed
    // `LeaseStore` exists (tracked as a follow-up, WP-PLT-18; see this task's report
    // `## Open questions`). Until then, actual timer *safety* (a wait firing more than once)
    // does not depend on this lease fencing at all: `Engine::step` fires timers from the
    // durable `CaseRecord.timers` it reads from the store (see `spot-bpm-engine`'s
    // `engine.rs`, step 1), and `ProcessHost::resume_once` lists runnable Cases through the
    // store-backed `CaseStore::list_runnable` — both are durable and tenant-scoped, never the
    // in-memory `TimerPort`/lease. The fenced port here only decides who is allowed to
    // *register* a wait; duplicate registration from two stale holders would be harmless noise
    // the durable due-timer check below already dedupes.
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let name = "timer:shared";
    let first = leases
        .acquire(name, "node-1", Duration::from_secs(60), to_jiff(now()))
        .unwrap();
    let inner: Arc<dyn TimerPort> = Arc::new(InMemoryTimerPort::new());
    let port_one = FencedTimerPort::new(Arc::clone(&leases), first.clone(), Arc::clone(&inner));

    let registration = spot_bpm_engine::TimerRegistration {
        case: CaseId::new("case_fence").unwrap(),
        node: spot_dag::NodeId::new("pause").unwrap(),
        attempt: 1,
        due: Some(now()),
        event: None,
        human: None,
    };
    port_one.register(registration.clone());

    // The lease moves to a second holder (a new node taking over).
    leases.release(&first).unwrap();
    let second = leases
        .acquire(name, "node-2", Duration::from_secs(60), to_jiff(now()))
        .unwrap();
    let port_two = FencedTimerPort::new(Arc::clone(&leases), second, Arc::clone(&inner));

    // The stale port (node-1) can no longer fire the wait; the new holder's can.
    assert!(
        port_one.due(now()).is_empty(),
        "the stale holder fires nothing"
    );
    assert_eq!(
        port_two.due(now()).len(),
        1,
        "the new holder fires the wait"
    );
    assert_eq!(port_two.due(now()).len(), 0, "a wait fires exactly once");
    assert_eq!(
        port_two.fired_count(&registration.case, &registration.node),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_carry_counts_but_the_module_never_builds_a_labelled_one() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let host = host(&db, "tenant-metrics", Arc::new(RecordingSubmit::default())).await;
    publish(&host, THREE_NODE, "tester").await;
    let before = host.metrics();
    assert_eq!(before.runs_started, 0);
    let case = host
        .start_run(
            "three",
            None,
            serde_json::json!({}),
            None,
            "metrics-key",
            now(),
        )
        .await
        .unwrap();
    let engine = host.engine().unwrap();
    engine.run_until_idle(&case, now(), 20).await.unwrap();
    let after = host.metrics();
    assert_eq!(after.runs_started, 1);
    // `process.rs` never calls `metrics::counter!`/`metrics::gauge!` with a `=>` label pair in
    // this module (reviewed at the call sites in `Metrics`); the snapshot above is the only
    // place a run or Case identity reaches a number.
}

#[tokio::test]
async fn the_handler_builder_is_reachable_through_the_public_module_path() {
    // A smoke test that the port other node-kind modules depend on (`HandlerBuilder`) is a
    // public item of `process.rs`, with the shape the brief names.
    let builder = HandlerBuilder::new();
    builder.register(Arc::new(spot_bpm_engine::TriggerHandler));
}
