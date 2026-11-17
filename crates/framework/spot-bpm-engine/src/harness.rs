//! The deterministic simulation harness: a virtual clock, an effect ledger and scripted crashes.
//!
//! The harness drives an in-memory [`Engine`] with no real time, no randomness and no I/O:
//!
//! * [`VirtualClock`] is the only source of "now", and only moves when a test moves it;
//! * [`EffectLedger`] records every side effect a handler performs, keyed by the attempt's
//!   [`IdempotencyKey`](crate::IdempotencyKey), so a test can prove a side effect happened
//!   exactly once across a crash and a resume;
//! * [`ScriptedFaults`] crashes the engine after the *n*-th effect boundary, so a test can sweep
//!   `n` over every boundary of a process and assert exactly-once side effects at each.
//!
//! [`Harness::drive`] steps a Case to completion, recovering from an injected crash by stepping
//! again — the engine's durable state is what makes that safe.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_model::ProcessVersion;

use crate::case::CaseRecord;
use crate::engine::{EffectPoint, Engine, Faults, InMemoryEngine, StepReport};
use crate::error::{EngineError, EngineResult};
use crate::handler::{CaseId, HandlerRegistry, NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use crate::node_run_log::NodeRunLogFactory;
use crate::store::InMemoryCaseStore;
use crate::timer::InMemoryTimerPort;

/// The fixed instant the harness starts at (2023-11-14T22:13:20Z); nothing reads a real clock.
pub const DEFAULT_START_SECONDS: i64 = 1_700_000_000;

/// The harness's fixed start instant.
#[must_use]
pub fn default_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(DEFAULT_START_SECONDS).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// A clock that only moves when a test moves it.
#[derive(Debug, Clone)]
pub struct VirtualClock {
    now: Arc<Mutex<OffsetDateTime>>,
}

impl VirtualClock {
    /// A clock at `start`.
    #[must_use]
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }

    /// The current instant.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Moves the clock forward by whole seconds and returns the new instant.
    pub fn advance(&self, seconds: u64) -> OffsetDateTime {
        let mut guard = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard += Duration::from_secs(seconds);
        *guard
    }
}

/// A ledger of side effects, keyed by the attempt's idempotency key.
///
/// A handler is idempotent by the key: it calls [`EffectLedger::claim`] and performs the side
/// effect only when the claim is new. [`EffectLedger::performed`] then counts how many times the
/// effect actually happened, so a test can prove it happened exactly once even when the handler
/// was called more than once (a replay after a crash).
#[derive(Debug, Default)]
pub struct EffectLedger {
    claimed: Mutex<BTreeMap<String, u64>>,
    performed: Mutex<BTreeMap<String, u64>>,
    order: Mutex<Vec<String>>,
}

impl EffectLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn claimed_lock(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        self.claimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn performed_lock(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        self.performed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claims the effect for a key. Returns `true` the first time the key is seen, `false` for a
    /// replay; a handler performs its side effect only on `true`.
    pub fn claim(&self, key: &str) -> bool {
        let mut claimed = self.claimed_lock();
        let first = !claimed.contains_key(key);
        *claimed.entry(key.to_owned()).or_insert(0) += 1;
        first
    }

    /// Records that the effect happened.
    pub fn perform(&self, key: &str) {
        let mut performed = self.performed_lock();
        let first = !performed.contains_key(key);
        *performed.entry(key.to_owned()).or_insert(0) += 1;
        drop(performed);
        if first {
            self.order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(key.to_owned());
        }
    }

    /// How many times a handler claimed a key (its call count).
    #[must_use]
    pub fn calls(&self, key: &str) -> u64 {
        self.claimed_lock().get(key).copied().unwrap_or(0)
    }

    /// How many times the effect for a key actually happened.
    #[must_use]
    pub fn performed(&self, key: &str) -> u64 {
        self.performed_lock().get(key).copied().unwrap_or(0)
    }

    /// How many effects happened in total.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.performed_lock().values().sum()
    }

    /// How many distinct effect keys happened.
    #[must_use]
    pub fn distinct(&self) -> usize {
        self.performed_lock().len()
    }

    /// The keys whose effect happened more than once (the exactly-once violation).
    #[must_use]
    pub fn duplicates(&self) -> Vec<String> {
        self.performed_lock()
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Every performed key, in first-performed order.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// A fault injector that crashes after the *n*-th effect boundary and then stays quiet.
#[derive(Debug)]
pub struct ScriptedFaults {
    crash_at: AtomicU64,
    seen: AtomicU64,
    fired: AtomicBool,
}

impl ScriptedFaults {
    /// An injector that never crashes.
    #[must_use]
    pub fn never() -> Self {
        Self {
            crash_at: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            fired: AtomicBool::new(false),
        }
    }

    /// An injector that crashes at the *n*-th boundary (1-based).
    #[must_use]
    pub fn after(effects: u64) -> Self {
        let faults = Self::never();
        faults.arm(effects);
        faults
    }

    /// Arms the injector to crash at the *n*-th boundary; `0` disarms it.
    pub fn arm(&self, effects: u64) {
        self.crash_at.store(effects, Ordering::SeqCst);
        self.fired.store(false, Ordering::SeqCst);
    }

    /// Disarms the injector.
    pub fn disarm(&self) {
        self.arm(0);
    }

    /// How many effect boundaries were seen.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen.load(Ordering::SeqCst)
    }

    /// Whether the injector has crashed once.
    #[must_use]
    pub fn has_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl Faults for ScriptedFaults {
    fn check(&self, point: &EffectPoint) -> EngineResult<()> {
        let seen = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
        let crash_at = self.crash_at.load(Ordering::SeqCst);
        if crash_at != 0 && seen == crash_at && !self.fired.swap(true, Ordering::SeqCst) {
            return Err(EngineError::Crashed {
                point: point.label(),
            });
        }
        Ok(())
    }
}

/// A handler that records one effect per attempt and always succeeds.
pub struct RecordingHandler {
    kind: NodeKind,
    ledger: Arc<EffectLedger>,
}

impl RecordingHandler {
    /// A handler for `kind` that records into `ledger`.
    #[must_use]
    pub fn new(kind: NodeKind, ledger: Arc<EffectLedger>) -> Self {
        Self { kind, ledger }
    }
}

impl NodeHandler for RecordingHandler {
    fn kind(&self) -> NodeKind {
        self.kind.clone()
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        // Idempotent by the attempt key: a replay performs nothing.
        let key = ctx.key.to_string();
        if self.ledger.claim(&key) {
            self.ledger.perform(&key);
        }
        NodeOutcome::Done {
            output: None,
            branches: ctx.outgoing_conditions(),
        }
    }
}

/// A handler that fails its first `fail_before` attempts and then records an effect and
/// succeeds, so a test can drive a retry and an error branch deterministically.
pub struct FlakyHandler {
    kind: NodeKind,
    ledger: Arc<EffectLedger>,
    fail_before: u32,
}

impl FlakyHandler {
    /// A handler for `kind` that fails while its attempt is at most `fail_before`.
    #[must_use]
    pub fn new(kind: NodeKind, ledger: Arc<EffectLedger>, fail_before: u32) -> Self {
        Self {
            kind,
            ledger,
            fail_before,
        }
    }
}

impl NodeHandler for FlakyHandler {
    fn kind(&self) -> NodeKind {
        self.kind.clone()
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        if ctx.attempt <= self.fail_before {
            return NodeOutcome::fail("scripted_failure", format!("attempt {}", ctx.attempt));
        }
        let key = ctx.key.to_string();
        if self.ledger.claim(&key) {
            self.ledger.perform(&key);
        }
        NodeOutcome::Done {
            output: None,
            branches: ctx.outgoing_conditions(),
        }
    }
}

/// The simulation harness: an in-memory engine, a virtual clock, an effect ledger and scripted
/// faults.
pub struct Harness {
    engine: InMemoryEngine,
    ledger: Arc<EffectLedger>,
    clock: VirtualClock,
    faults: Arc<ScriptedFaults>,
}

impl Harness {
    /// A harness with the given handlers and its own effect ledger.
    #[must_use]
    pub fn new(handlers: HandlerRegistry) -> Self {
        Self::with_ledger(handlers, Arc::new(EffectLedger::new()))
    }

    /// A harness whose handlers and tests share one effect ledger.
    #[must_use]
    pub fn with_ledger(handlers: HandlerRegistry, ledger: Arc<EffectLedger>) -> Self {
        let clock = VirtualClock::new(default_now());
        let faults = Arc::new(ScriptedFaults::never());
        let engine = Engine::new(InMemoryCaseStore::new(), InMemoryTimerPort::new(), handlers)
            .with_faults(faults.clone());
        Self {
            engine,
            ledger,
            clock,
            faults,
        }
    }

    /// The engine under test.
    #[must_use]
    pub fn engine(&self) -> &InMemoryEngine {
        &self.engine
    }

    /// The effect ledger.
    #[must_use]
    pub fn ledger(&self) -> Arc<EffectLedger> {
        self.ledger.clone()
    }

    /// The virtual clock.
    #[must_use]
    pub fn clock(&self) -> &VirtualClock {
        &self.clock
    }

    /// The scripted faults.
    #[must_use]
    pub fn faults(&self) -> Arc<ScriptedFaults> {
        self.faults.clone()
    }

    /// Arms the injector to crash at the *n*-th effect boundary.
    pub fn crash_after(&self, effects: u64) {
        self.faults.arm(effects);
    }

    /// Disarms the injector.
    pub fn no_crash(&self) {
        self.faults.disarm();
    }

    /// Starts a Case at the clock's current instant.
    ///
    /// # Errors
    /// A model problem, a duplicate id or key, or a store failure.
    pub async fn start(
        &self,
        version: &ProcessVersion,
        id: CaseId,
        key: &str,
    ) -> EngineResult<CaseRecord> {
        self.engine
            .start(version, id, key.to_owned(), None, self.clock.now())
            .await
    }

    /// Steps a Case once at the clock's current instant.
    ///
    /// # Errors
    /// A store failure, a missing process or handler, or an injected crash.
    pub async fn step(&self, id: &CaseId) -> EngineResult<StepReport> {
        self.engine.step(id, self.clock.now()).await
    }

    /// Steps a Case (and its sub-process children) to completion, recovering from injected
    /// crashes and moving the virtual clock forward when a turn makes no progress (so a due
    /// wait fires).
    ///
    /// # Errors
    /// Any engine error other than [`EngineError::Crashed`].
    pub async fn drive(&self, id: &CaseId, max_turns: usize) -> EngineResult<CaseRecord> {
        let mut crashes = 0_u32;
        for _ in 0..max_turns {
            let record = self.engine.load(id).await?;
            if record.run.status != spot_dag_exec::RunStatus::Running {
                return Ok(record);
            }
            let mut progressed = false;
            match self.engine.step(id, self.clock.now()).await {
                Ok(report) => progressed |= report.made_progress(),
                Err(EngineError::Crashed { .. }) => {
                    crashes += 1;
                    if crashes > 10_000 {
                        return Err(EngineError::Crashed {
                            point: "too many injected crashes".to_owned(),
                        });
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
            // Step each sub-process child one turn too, so the parent can resume exactly once
            // when the child finishes.
            let record = self.engine.load(id).await?;
            for link in record.children.values() {
                match self.engine.step(&link.child, self.clock.now()).await {
                    Ok(report) => progressed |= report.made_progress(),
                    Err(EngineError::Crashed { .. }) => {
                        crashes += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
            let record = self.engine.load(id).await?;
            if record.run.status != spot_dag_exec::RunStatus::Running {
                return Ok(record);
            }
            if !progressed {
                self.clock.advance(3600);
            }
        }
        self.engine.load(id).await
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new(HandlerRegistry::logic_default())
    }
}

/// A failed contract check: the plain-language statement that did not hold.
fn ensure(condition: bool, what: &str) -> EngineResult<()> {
    if condition {
        Ok(())
    } else {
        Err(EngineError::StoreFailed {
            detail: format!("node-run log contract: {what}"),
        })
    }
}

/// The node-run log contract a backend must pass: the same checks run against the in-memory
/// store and the PostgreSQL store (WP-BPM-11.T6).
///
/// It checks commit-order `seq`, per-Case order, that a rolled-back or refused commit and a
/// replayed one leave no record, gap-free paging, the checkpoint compare-and-set, consumer-name
/// validation and that one tenant never sees another's records or checkpoints.
///
/// # Errors
/// [`EngineError::StoreFailed`] naming the first check that did not hold, or the store's own
/// error when an operation that must succeed fails.
pub async fn node_run_log_contract(factory: &dyn NodeRunLogFactory) -> EngineResult<()> {
    use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId};
    use spot_dag_exec::{Instant, NodeOutcome as DagOutcome, claim, complete};

    use crate::case::{Attempt, AttemptState, CaseUpdate, TimerRecord};
    use crate::node_run_log::{Checkpoint, LoggedState, LoggedWait, NodeRunSeq};

    let invalid = |detail: String| EngineError::StoreFailed { detail };
    let nid = |raw: &str| NodeId::new(raw).map_err(|e| invalid(e.to_string()));
    let (n0, n1) = (nid("n0")?, nid("n1")?);
    let nodes = vec![
        Node::new(n0.clone(), "step", JoinRule::All, serde_json::Value::Null)
            .map_err(|e| invalid(e.to_string()))?,
        Node::new(n1.clone(), "step", JoinRule::All, serde_json::Value::Null)
            .map_err(|e| invalid(e.to_string()))?,
    ];
    let edges = vec![Edge::new(n0.clone(), n1.clone(), None)];
    let dag = CompiledDag::compile(
        &DagDefinition::new("contract", 1, nodes, edges),
        &Limits::new(),
    )
    .map_err(|e| invalid(format!("{e:?}")))?;
    let published = spot_dag::Published {
        version: dag.version(),
        hash: dag.hash().clone(),
    };
    let t0 = default_now();
    let at = |seconds: u64| Instant::from_offset(t0 + Duration::from_secs(seconds));

    let a = factory.open("tenant_a");
    let b = factory.open("tenant_b");
    let case = CaseId::new("log_case").map_err(|e| invalid(e.to_string()))?;
    let plan = spot_dag_exec::start(
        &dag,
        &published,
        spot_dag_exec::RunId::new("log_case").map_err(|e| invalid(e.to_string()))?,
        "log-key",
        t0,
    )
    .map_err(|e| invalid(e.to_string()))?;
    let mut record = a.create(CaseRecord::new(plan.run, None)).await?;
    ensure(
        a.list_after(NodeRunSeq(0), 10).await?.is_empty(),
        "a fresh store has an empty log",
    )?;

    // A claim logs `started` at the step's own instant.
    let step = claim(&record.run, &n0, t0).map_err(|e| invalid(e.to_string()))?;
    record = a
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new().with_transition(step),
        )
        .await?;
    ensure(
        a.list_after(NodeRunSeq(0), 10).await?.len() == 1,
        "a claim logs one record",
    )?;

    // A rolled-back commit (stale version) and a refused one (no instant) leave nothing.
    let stale = CaseUpdate::new()
        .with_attempt(Attempt::started(n0.clone(), 1, at(1), false))
        .with_at(at(1));
    ensure(
        a.commit(&case, record.run.version + 9, stale)
            .await
            .is_err(),
        "a stale version is refused",
    )?;
    let no_instant = CaseUpdate::new().with_attempt(Attempt::started(n0.clone(), 1, at(1), false));
    ensure(
        a.commit(&case, record.run.version, no_instant)
            .await
            .is_err(),
        "an attempt without an instant is refused",
    )?;
    ensure(
        a.list_after(NodeRunSeq(0), 10).await?.len() == 1,
        "refused commits leave no record",
    )?;

    // Done attempt, then the DAG completion: one `done` (the completion repeats the attempt).
    let mut done = Attempt::started(n0.clone(), 1, at(0), false);
    done.state = AttemptState::Done;
    record = a
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new().with_attempt(done.clone()).with_at(at(5)),
        )
        .await?;
    let finish = complete(
        &dag,
        &record.run,
        &n0,
        DagOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        t0 + Duration::from_secs(5),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let before_replay = record.run.version;
    record = a
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new().with_transition(finish.clone()),
        )
        .await?;
    // Replays: the same transition and the same attempt again.
    a.commit(
        &case,
        before_replay,
        CaseUpdate::new().with_transition(finish),
    )
    .await?;
    a.commit(
        &case,
        record.run.version,
        CaseUpdate::new().with_attempt(done).with_at(at(6)),
    )
    .await?;
    ensure(
        a.list_after(NodeRunSeq(0), 10).await?.len() == 2,
        "a replayed commit adds no record",
    )?;

    // n1: claim, a retry wait, attempt 2 started and done, then a human wait on another attempt.
    let step = claim(&record.run, &n1, t0 + Duration::from_secs(10))
        .map_err(|e| invalid(e.to_string()))?;
    record = a
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new().with_transition(step),
        )
        .await?;
    let mut retrying = Attempt::started(n1.clone(), 1, at(10), false);
    retrying.state = AttemptState::Retrying;
    retrying.failure = Some("integration.down: SECRET-TEXT".to_owned());
    record = a
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new().with_attempt(retrying).with_at(at(11)),
        )
        .await?;
    let second = Attempt::started(n1.clone(), 2, at(12), false);
    record = a
        .commit(
            &case,
            record.run.version,
            CaseUpdate::new()
                .with_attempt(second.clone())
                .with_at(at(12)),
        )
        .await?;
    let mut waiting = second;
    waiting.state = AttemptState::Waiting;
    let timer = TimerRecord {
        node: n1.clone(),
        attempt: 2,
        due: None,
        event: None,
        human: Some("approve".to_owned()),
    };
    a.commit(
        &case,
        record.run.version,
        CaseUpdate::new()
            .with_attempt(waiting)
            .with_timer(timer)
            .with_at(at(13)),
    )
    .await?;

    let all = a.list_after(NodeRunSeq(0), 1000).await?;
    let shape: Vec<(String, u32, LoggedState)> = all
        .iter()
        .map(|r| (r.node.to_string(), r.attempt, r.state))
        .collect();
    ensure(
        shape
            == vec![
                ("n0".to_owned(), 1, LoggedState::Started),
                ("n0".to_owned(), 1, LoggedState::Done),
                ("n1".to_owned(), 1, LoggedState::Started),
                ("n1".to_owned(), 1, LoggedState::Waiting),
                ("n1".to_owned(), 2, LoggedState::Started),
                ("n1".to_owned(), 2, LoggedState::Waiting),
            ],
        "the log holds exactly the committed transitions in commit order",
    )?;
    ensure(
        all.windows(2).all(|w| w[0].seq < w[1].seq),
        "seq is strictly increasing",
    )?;
    ensure(
        all[3].wait == Some(LoggedWait::IntegrationRetry) && all[3].is_retry,
        "a retry is an integration_retry wait",
    )?;
    ensure(
        all[5].wait == Some(LoggedWait::HumanTask) && all[5].human.as_deref() == Some("approve"),
        "a human wait carries its task key",
    )?;
    ensure(
        !format!("{all:?}").contains("SECRET-TEXT"),
        "no record holds the plain-language failure text",
    )?;
    ensure(
        all[1].at == at(5) && all[3].at == at(11),
        "records carry the commit instants",
    )?;
    ensure(
        all.iter()
            .all(|r| r.dag_key == "contract" && r.dag_version == 1),
        "records name the process",
    )?;

    // Paging by two follows the cursor with no gap and no repeat; a zero limit returns nothing.
    let mut paged = Vec::new();
    let mut cursor = NodeRunSeq(0);
    loop {
        let page = a.list_after(cursor, 2).await?;
        ensure(page.len() <= 2, "a page respects its limit")?;
        let Some(last) = page.last() else { break };
        cursor = last.seq;
        paged.extend(page);
    }
    ensure(paged == all, "paging has no gap and no repeat")?;
    ensure(
        a.list_after(NodeRunSeq(0), 0).await?.is_empty(),
        "a zero limit returns nothing",
    )?;

    // The checkpoint is a compare-and-set on its stored seq.
    ensure(
        a.load_checkpoint("insight").await?.is_none(),
        "no checkpoint at first",
    )?;
    let first = Checkpoint {
        seq: all[2].seq,
        state: serde_json::json!({"n": 3}),
    };
    ensure(
        a.save_checkpoint("insight", NodeRunSeq(0), first.clone())
            .await?,
        "the first save from 0 wins",
    )?;
    let stale = Checkpoint {
        seq: all[5].seq,
        state: serde_json::json!({"n": 6}),
    };
    ensure(
        !a.save_checkpoint("insight", NodeRunSeq(0), stale).await?,
        "a stale expected seq loses",
    )?;
    ensure(
        a.load_checkpoint("insight").await? == Some(first),
        "the stale save changed nothing",
    )?;
    ensure(
        a.save_checkpoint(
            "insight",
            all[2].seq,
            Checkpoint {
                seq: all[5].seq,
                state: serde_json::Value::Null,
            },
        )
        .await?,
        "the current expected seq wins",
    )?;
    ensure(
        a.load_checkpoint("Bad Name").await.is_err(),
        "an invalid consumer name is refused",
    )?;

    // Tenant isolation: B sees nothing of A, and B's prune and checkpoints do not touch A.
    ensure(
        b.list_after(NodeRunSeq(0), 1000).await?.is_empty(),
        "another tenant's log is empty",
    )?;
    ensure(
        b.load_checkpoint("insight").await?.is_none(),
        "another tenant has no checkpoint",
    )?;
    ensure(
        b.prune_before(NodeRunSeq(1000)).await? == 0,
        "another tenant prunes nothing of ours",
    )?;
    ensure(
        a.list_after(NodeRunSeq(0), 1000).await?.len() == 6,
        "our log survived",
    )?;

    // Prune deletes only records below the cut.
    let removed = a.prune_before(all[3].seq).await?;
    ensure(removed == 3, "prune removes the records below the cut")?;
    ensure(
        a.list_after(NodeRunSeq(0), 1000)
            .await?
            .first()
            .map(|r| r.seq)
            == Some(all[3].seq),
        "the records at and above the cut remain",
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_only_moves_when_moved() {
        let clock = VirtualClock::new(default_now());
        let start = clock.now();
        assert_eq!(clock.now(), start);
        assert_eq!(clock.advance(60), start + Duration::from_secs(60));
    }

    #[test]
    fn the_ledger_detects_duplicates() {
        let ledger = EffectLedger::new();
        assert!(ledger.claim("a"));
        ledger.perform("a");
        // A replay claims the same key again but performs nothing.
        assert!(!ledger.claim("a"));
        if ledger.claim("a") {
            ledger.perform("a");
        }
        assert!(ledger.claim("b"));
        ledger.perform("b");
        assert_eq!(ledger.calls("a"), 3);
        assert_eq!(ledger.performed("a"), 1);
        assert_eq!(ledger.total(), 2);
        assert_eq!(ledger.distinct(), 2);
        assert!(ledger.duplicates().is_empty());
    }

    #[test]
    fn scripted_faults_crash_once() {
        let faults = ScriptedFaults::after(2);
        let point = EffectPoint::BeforeHandler {
            case: CaseId::new("case1").unwrap(),
            node: spot_dag::NodeId::new("n1").unwrap(),
            attempt: 1,
        };
        assert!(faults.check(&point).is_ok());
        assert!(matches!(
            faults.check(&point),
            Err(EngineError::Crashed { .. })
        ));
        assert!(faults.check(&point).is_ok(), "it crashes only once");
    }
}
