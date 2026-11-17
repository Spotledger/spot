//! The node-run transition log: a durable, ordered feed of what every node attempt did and when.
//!
//! The Case tables keep one row per `(case, node)` and are overwritten as a node moves, so they
//! cannot say *when* an attempt waited, finished or failed. This module defines the storage
//! agnostic half of an append-only log that can:
//!
//! * [`derive_node_runs`] is the **one pure function** every store uses to turn a committed
//!   [`CaseUpdate`] into [`NodeRunDraft`]s, so the in-memory store and the PostgreSQL store can
//!   never disagree on what an update means;
//! * [`NodeRunLog`] is the reader port: ordered paging by [`NodeRunSeq`], a compare-and-set
//!   [`Checkpoint`] per consumer and pruning;
//! * [`NodeRunLogFactory`] lets one contract test (`harness::node_run_log_contract`) run against
//!   every backend.
//!
//! # Guarantees
//!
//! A store that implements [`NodeRunLog`] guarantees, and the contract test asserts:
//!
//! 1. `seq` is strictly increasing per tenant in commit order, so a reader that has seen `N`
//!    never later finds a committed record with `seq < N`;
//! 2. the records of one Case appear in the order its commits happened;
//! 3. a rolled-back commit (a version conflict, a refused update) leaves no record;
//! 4. a replayed commit (the exactly-once guard kept an earlier outcome, or a transition was
//!    already applied) adds none;
//! 5. a consumer name is `[a-z0-9_.-]{1,64}` and is validated before use.
//!
//! A record holds codes, keys, ids and instants only: never an attempt's `output`, the Case
//! variables or the plain-language failure text. Nothing here reads a clock; the commit instant
//! is [`CaseUpdate::at`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_dag::NodeId;
use spot_dag_exec::{Completion, Instant, Step};

use crate::case::{AttemptState, CaseUpdate};
use crate::error::{EngineError, EngineResult};
use crate::handler::CaseId;
use crate::store::{CaseFuture, CaseStore};

/// The most records one [`NodeRunLog::list_after`] call returns.
pub const MAX_PAGE: u32 = 1000;

/// The stable code recorded for a failure whose text carries no machine code.
pub const UNSPECIFIED_FAILURE_CODE: &str = "bpm_engine.node_failed";

/// Where a node attempt is after one logged transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoggedState {
    /// The handler is running.
    Started,
    /// The attempt paused (a timer, an event, a person or its retry).
    Waiting,
    /// The attempt succeeded.
    Done,
    /// The attempt failed terminally (the error branch is taken).
    Failed,
    /// The Case was cancelled while the attempt was open.
    Cancelled,
}

impl LoggedState {
    /// The stable token a store keeps.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The state a stored token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "started" => Some(Self::Started),
            "waiting" => Some(Self::Waiting),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Why a logged attempt is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoggedWait {
    /// A timer or a date (a `due` instant).
    Timer,
    /// An event key must arrive.
    Event,
    /// A person must act (a human-task key).
    HumanTask,
    /// An integration failed and the attempt waits for its retry.
    IntegrationRetry,
}

impl LoggedWait {
    /// The stable token a store keeps.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Timer => "timer",
            Self::Event => "event",
            Self::HumanTask => "human_task",
            Self::IntegrationRetry => "integration_retry",
        }
    }

    /// The reason a stored token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "timer" => Some(Self::Timer),
            "event" => Some(Self::Event),
            "human_task" => Some(Self::HumanTask),
            "integration_retry" => Some(Self::IntegrationRetry),
            _ => None,
        }
    }
}

/// One derived node-run transition, before a store assigns it a sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunDraft {
    /// The node.
    pub node: NodeId,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
    /// Where the attempt is after the transition.
    pub state: LoggedState,
    /// When the transition happened.
    pub at: Instant,
    /// Why the attempt waits, for a `Waiting` transition.
    pub wait: Option<LoggedWait>,
    /// The stable failure code, for a `Failed` transition. Never the plain-language text.
    pub failure_code: Option<String>,
    /// Whether the attempt replays an unfinished one (or is the wait for a retry).
    pub is_retry: bool,
    /// The human-task key a `human_task` wait is for. A key, never a person: the consumer
    /// resolves the assignee.
    pub human: Option<String>,
}

impl NodeRunDraft {
    fn new(node: &NodeId, attempt: u32, state: LoggedState, at: Instant) -> Self {
        Self {
            node: node.clone(),
            attempt,
            state,
            at,
            wait: None,
            failure_code: None,
            is_retry: false,
            human: None,
        }
    }
}

/// A node that was open (running) when a commit applied, for a `Cancel` step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenNode {
    /// The open node.
    pub node: NodeId,
    /// The attempt that was open.
    pub attempt: u32,
}

/// What the store found while applying an update; it lets [`derive_node_runs`] stay pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEffect {
    /// `true` when the exactly-once guard kept an earlier (higher-numbered) attempt over the
    /// update's attempt: a replayed commit, which logs nothing for the attempt.
    pub kept_earlier_attempt: bool,
    /// `false` when the transition had already been applied (a replay): it logs nothing.
    pub transition_applied: bool,
    /// The nodes open before the transition, used by a `Cancel` step.
    pub open: Vec<OpenNode>,
    /// The engine attempt number stored for each node, after the update. A step carries the
    /// DAG's own attempt count, which stays at 1 across engine retries, so the engine's number
    /// wins when there is one.
    pub attempts: BTreeMap<NodeId, u32>,
}

impl StoredEffect {
    /// An effect where everything in the update was stored and nothing was open.
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            kept_earlier_attempt: false,
            transition_applied: true,
            open: Vec::new(),
            attempts: BTreeMap::new(),
        }
    }
}

/// Refuses an update whose attempt or timer carries no instant: a library never guesses a time.
///
/// # Errors
/// [`EngineError::StoreFailed`] when the update has an attempt or a timer and no [`CaseUpdate::at`].
pub fn require_instant(update: &CaseUpdate) -> EngineResult<Instant> {
    match update.at {
        Some(at) => Ok(at),
        None if update.attempt.is_some() || update.timer.is_some() => {
            Err(EngineError::StoreFailed {
                detail: "an update with an attempt or a timer needs a commit instant (`at`)"
                    .to_owned(),
            })
        }
        // Nothing in the update needs an instant; the caller's value is never read.
        None => Ok(Instant {
            seconds: 0,
            nanos: 0,
        }),
    }
}

/// The stable code of a failure text.
///
/// The engine writes a handler failure as `<code>: <reason>`. When the text starts with a code
/// of `[a-z][a-z0-9_.]{0,63}` followed by `": "`, that code is returned; any other text (a
/// timeout, a child failure) is recorded under [`UNSPECIFIED_FAILURE_CODE`]. The reason is
/// never kept.
#[must_use]
pub fn failure_code_of(text: Option<&str>) -> String {
    let code = text.and_then(|text| text.split_once(": ")).map(|(c, _)| c);
    match code {
        Some(code)
            if (1..=64).contains(&code.len())
                && code.starts_with(|c: char| c.is_ascii_lowercase())
                && code.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
                }) =>
        {
            code.to_owned()
        }
        _ => UNSPECIFIED_FAILURE_CODE.to_owned(),
    }
}

/// Derives the node-run transitions one committed update caused.
///
/// The rules (each has a unit test):
///
/// * a `Claim` step is `started`;
/// * a stored attempt is the matching event at [`CaseUpdate::at`]: `Started`, `Done`, `Failed`
///   (with the stable failure code), `Waiting` (with `wait` from the update's timer: a `due` is
///   `timer`, an event key `event`, a human key `human_task` with the key in `human`) and
///   `Retrying`, which is `waiting` with `integration_retry` and `is_retry`;
/// * a `Complete` step is `done`, or terminal `failed` with the code of the reason;
/// * a `Cancel` step is `cancelled` for every node that was open;
/// * nothing is produced for an attempt the exactly-once guard dropped, or for a transition that
///   was already applied.
///
/// Drafts are unique per `(node, attempt, state)`. `output` and the variables are never read.
///
/// # Errors
/// [`EngineError::StoreFailed`] when the update has an attempt or timer and no instant.
pub fn derive_node_runs(
    update: &CaseUpdate,
    stored: &StoredEffect,
) -> EngineResult<Vec<NodeRunDraft>> {
    let at = require_instant(update)?;
    let mut drafts: Vec<NodeRunDraft> = Vec::new();
    let mut push = |draft: NodeRunDraft| {
        let seen = drafts
            .iter()
            .any(|d| d.node == draft.node && d.attempt == draft.attempt && d.state == draft.state);
        if !seen {
            drafts.push(draft);
        }
    };

    if let Some(attempt) = &update.attempt
        && !stored.kept_earlier_attempt
    {
        let timer = update
            .timer
            .as_ref()
            .filter(|t| t.node == attempt.node && t.attempt == attempt.attempt);
        let mut draft = match attempt.state {
            AttemptState::Started => {
                NodeRunDraft::new(&attempt.node, attempt.attempt, LoggedState::Started, at)
            }
            AttemptState::Waiting | AttemptState::Retrying => {
                NodeRunDraft::new(&attempt.node, attempt.attempt, LoggedState::Waiting, at)
            }
            AttemptState::Done => {
                NodeRunDraft::new(&attempt.node, attempt.attempt, LoggedState::Done, at)
            }
            AttemptState::Failed => {
                NodeRunDraft::new(&attempt.node, attempt.attempt, LoggedState::Failed, at)
            }
        };
        draft.is_retry = attempt.is_retry;
        match attempt.state {
            AttemptState::Waiting => {
                if let Some(timer) = timer {
                    if let Some(key) = &timer.human {
                        draft.wait = Some(LoggedWait::HumanTask);
                        draft.human = Some(key.clone());
                    } else if timer.event.is_some() {
                        draft.wait = Some(LoggedWait::Event);
                    } else if timer.due.is_some() {
                        draft.wait = Some(LoggedWait::Timer);
                    }
                }
            }
            AttemptState::Retrying => {
                draft.wait = Some(LoggedWait::IntegrationRetry);
                draft.is_retry = true;
            }
            AttemptState::Failed => {
                draft.failure_code = Some(failure_code_of(attempt.failure.as_deref()));
            }
            AttemptState::Started | AttemptState::Done => {}
        }
        push(draft);
    }

    if let Some(transition) = &update.transition
        && stored.transition_applied
    {
        match &transition.step {
            Step::Claim {
                node,
                attempt,
                at: step_at,
            } => push(NodeRunDraft::new(
                node,
                *attempt,
                LoggedState::Started,
                *step_at,
            )),
            Step::Complete {
                node,
                attempt,
                completion,
                at: step_at,
                ..
            } => {
                let number = stored.attempts.get(node).copied().unwrap_or(*attempt);
                match completion {
                    Completion::Done { .. } => {
                        push(NodeRunDraft::new(node, number, LoggedState::Done, *step_at));
                    }
                    Completion::Failed { reason } => {
                        let mut draft =
                            NodeRunDraft::new(node, number, LoggedState::Failed, *step_at);
                        draft.failure_code = Some(failure_code_of(Some(reason)));
                        push(draft);
                    }
                }
            }
            Step::Cancel { at: step_at } => {
                for open in &stored.open {
                    push(NodeRunDraft::new(
                        &open.node,
                        open.attempt,
                        LoggedState::Cancelled,
                        *step_at,
                    ));
                }
            }
        }
    }
    Ok(drafts)
}

/// A position in one tenant's node-run log. `0` is "from the start".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeRunSeq(pub i64);

/// One committed node-run transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunRecord {
    /// The log position: strictly increasing per tenant in commit order.
    pub seq: NodeRunSeq,
    /// The Case.
    pub case: CaseId,
    /// The process (DAG key) the Case pins.
    pub dag_key: String,
    /// The process version the Case pins.
    pub dag_version: u32,
    /// The Case's version after the commit that produced the record.
    pub case_version: u64,
    /// The node.
    pub node: NodeId,
    /// The engine attempt number.
    pub attempt: u32,
    /// Where the attempt is after the transition.
    pub state: LoggedState,
    /// When the transition happened.
    pub at: Instant,
    /// Why the attempt waits, for `Waiting`.
    pub wait: Option<LoggedWait>,
    /// The stable failure code, for `Failed`.
    pub failure_code: Option<String>,
    /// Whether the attempt replays an unfinished one, or is the wait for a retry.
    pub is_retry: bool,
    /// The human-task key of a `human_task` wait.
    pub human: Option<String>,
}

impl NodeRunRecord {
    /// A record for `draft`, committed as `seq` on a Case at `case_version`.
    #[must_use]
    pub fn new(
        seq: NodeRunSeq,
        case: CaseId,
        dag_key: String,
        dag_version: u32,
        case_version: u64,
        draft: NodeRunDraft,
    ) -> Self {
        Self {
            seq,
            case,
            dag_key,
            dag_version,
            case_version,
            node: draft.node,
            attempt: draft.attempt,
            state: draft.state,
            at: draft.at,
            wait: draft.wait,
            failure_code: draft.failure_code,
            is_retry: draft.is_retry,
            human: draft.human,
        }
    }
}

/// A consumer's saved position and folded state.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    /// The last sequence number the consumer folded.
    pub seq: NodeRunSeq,
    /// The consumer's own serialised state.
    pub state: serde_json::Value,
}

/// Validates a consumer name: `[a-z0-9_.-]{1,64}`.
///
/// # Errors
/// [`EngineError::StoreFailed`] for any other name.
pub fn validate_consumer(consumer: &str) -> EngineResult<()> {
    let ok = (1..=64).contains(&consumer.len())
        && consumer.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(EngineError::StoreFailed {
            detail: "a consumer name is 1 to 64 characters of [a-z0-9_.-]".to_owned(),
        })
    }
}

/// The reader port of the node-run log.
///
/// A store is built for **one tenant**; no method takes a tenant, so a caller cannot read
/// another tenant's records. See the module docs for the five guarantees.
pub trait NodeRunLog: Send + Sync {
    /// Up to `min(limit, 1000)` records with `seq > after`, ordered by `seq`.
    fn list_after<'a>(
        &'a self,
        after: NodeRunSeq,
        limit: u32,
    ) -> CaseFuture<'a, Vec<NodeRunRecord>>;

    /// The consumer's checkpoint, when it has saved one.
    fn load_checkpoint<'a>(&'a self, consumer: &'a str) -> CaseFuture<'a, Option<Checkpoint>>;

    /// Saves `next` when the stored checkpoint's `seq` is still `expected` (`0` when none is
    /// stored). Returns `false` when another writer moved it, so a lease holder that lost its
    /// lease cannot overwrite a newer checkpoint. A `next` behind `expected` is refused.
    fn save_checkpoint<'a>(
        &'a self,
        consumer: &'a str,
        expected: NodeRunSeq,
        next: Checkpoint,
    ) -> CaseFuture<'a, bool>;

    /// Deletes this tenant's records with `seq < before` and returns how many. Never scheduled
    /// by the engine.
    fn prune_before<'a>(&'a self, before: NodeRunSeq) -> CaseFuture<'a, u64>;
}

/// A [`CaseStore`] that also keeps the node-run log: what the contract test drives.
pub trait LoggedCaseStore: CaseStore + NodeRunLog {}

impl<T: CaseStore + NodeRunLog> LoggedCaseStore for T {}

/// A factory for logged stores over one backend, one per tenant.
pub trait NodeRunLogFactory: Send + Sync {
    /// A store over the factory's backend, bound to `tenant`. The first call for a tenant
    /// returns an empty store; later calls see the same data.
    fn open(&self, tenant: &str) -> Box<dyn LoggedCaseStore>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::case::{Attempt, TimerRecord};
    use spot_dag_exec::{RunId, RunStatus, Transition};
    use std::collections::BTreeSet;

    fn nid(raw: &str) -> NodeId {
        NodeId::new(raw).unwrap()
    }

    fn at(seconds: i64) -> Instant {
        Instant { seconds, nanos: 0 }
    }

    fn attempt(state: AttemptState) -> Attempt {
        let mut a = Attempt::started(nid("n"), 1, at(10), false);
        a.state = state;
        a
    }

    fn derive(update: &CaseUpdate) -> Vec<NodeRunDraft> {
        derive_node_runs(update, &StoredEffect::fresh()).unwrap()
    }

    fn step(step: Step) -> Transition {
        Transition {
            run: RunId::new("case1").unwrap(),
            step,
        }
    }

    fn complete_step(completion: Completion, seconds: i64) -> Transition {
        step(Step::Complete {
            node: nid("n"),
            attempt: 1,
            completion,
            at: at(seconds),
            taken: BTreeSet::new(),
            ready: Vec::new(),
            skipped: Vec::new(),
            status: RunStatus::Running,
            ended_at: None,
        })
    }

    #[test]
    fn a_claim_step_is_started() {
        let update = CaseUpdate::new().with_transition(step(Step::Claim {
            node: nid("n"),
            attempt: 1,
            at: at(5),
        }));
        let drafts = derive(&update);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].state, LoggedState::Started);
        assert_eq!(drafts[0].at, at(5));
    }

    #[test]
    fn started_and_done_attempts_use_the_commit_instant() {
        for (state, expected) in [
            (AttemptState::Started, LoggedState::Started),
            (AttemptState::Done, LoggedState::Done),
        ] {
            let update = CaseUpdate::new()
                .with_attempt(attempt(state))
                .with_at(at(20));
            let drafts = derive(&update);
            assert_eq!(drafts.len(), 1);
            assert_eq!(drafts[0].state, expected);
            assert_eq!(drafts[0].at, at(20));
            assert_eq!(drafts[0].wait, None);
        }
    }

    #[test]
    fn a_waiting_attempt_names_its_reason() {
        let cases = [
            (
                TimerRecord {
                    node: nid("n"),
                    attempt: 1,
                    due: Some(at(99)),
                    event: None,
                    human: None,
                },
                Some(LoggedWait::Timer),
                None,
            ),
            (
                TimerRecord {
                    node: nid("n"),
                    attempt: 1,
                    due: None,
                    event: Some("paid".to_owned()),
                    human: None,
                },
                Some(LoggedWait::Event),
                None,
            ),
            (
                TimerRecord {
                    node: nid("n"),
                    attempt: 1,
                    due: None,
                    event: None,
                    human: Some("approve".to_owned()),
                },
                Some(LoggedWait::HumanTask),
                Some("approve".to_owned()),
            ),
        ];
        for (timer, wait, human) in cases {
            let update = CaseUpdate::new()
                .with_attempt(attempt(AttemptState::Waiting))
                .with_timer(timer)
                .with_at(at(30));
            let drafts = derive(&update);
            assert_eq!(drafts.len(), 1);
            assert_eq!(drafts[0].state, LoggedState::Waiting);
            assert_eq!(drafts[0].wait, wait);
            assert_eq!(drafts[0].human, human);
        }
    }

    #[test]
    fn a_retrying_attempt_is_an_integration_retry_wait() {
        let mut a = attempt(AttemptState::Retrying);
        a.failure = Some("integration.down: secret text".to_owned());
        let drafts = derive(&CaseUpdate::new().with_attempt(a).with_at(at(40)));
        assert_eq!(drafts[0].state, LoggedState::Waiting);
        assert_eq!(drafts[0].wait, Some(LoggedWait::IntegrationRetry));
        assert!(drafts[0].is_retry);
        assert_eq!(drafts[0].failure_code, None);
    }

    #[test]
    fn a_failed_attempt_carries_the_code_never_the_text() {
        let mut a = attempt(AttemptState::Failed);
        a.failure = Some("erp.rejected: SECRET-REASON".to_owned());
        let drafts = derive(&CaseUpdate::new().with_attempt(a).with_at(at(50)));
        assert_eq!(drafts[0].failure_code.as_deref(), Some("erp.rejected"));
        assert!(!format!("{drafts:?}").contains("SECRET"));
        let mut b = attempt(AttemptState::Failed);
        b.failure = Some("the node exceeded its 5s timeout".to_owned());
        let drafts = derive(&CaseUpdate::new().with_attempt(b).with_at(at(50)));
        assert_eq!(
            drafts[0].failure_code.as_deref(),
            Some(UNSPECIFIED_FAILURE_CODE)
        );
    }

    #[test]
    fn a_complete_step_is_done_or_failed() {
        let done = CaseUpdate::new().with_transition(complete_step(
            Completion::Done {
                output: Some(serde_json::json!({"secret": "OUTPUT"})),
            },
            60,
        ));
        let drafts = derive(&done);
        assert_eq!(drafts[0].state, LoggedState::Done);
        assert_eq!(drafts[0].at, at(60));
        assert!(!format!("{drafts:?}").contains("OUTPUT"));
        let failed = CaseUpdate::new().with_transition(complete_step(
            Completion::Failed {
                reason: "bad.input: SECRET".to_owned(),
            },
            61,
        ));
        let drafts = derive(&failed);
        assert_eq!(drafts[0].state, LoggedState::Failed);
        assert_eq!(drafts[0].failure_code.as_deref(), Some("bad.input"));
    }

    #[test]
    fn a_complete_step_uses_the_engine_attempt_number() {
        let update =
            CaseUpdate::new().with_transition(complete_step(Completion::Done { output: None }, 70));
        let mut stored = StoredEffect::fresh();
        stored.attempts.insert(nid("n"), 2);
        let drafts = derive_node_runs(&update, &stored).unwrap();
        assert_eq!(drafts[0].attempt, 2);
    }

    #[test]
    fn a_cancel_step_cancels_each_open_node() {
        let update = CaseUpdate::new().with_transition(step(Step::Cancel { at: at(80) }));
        let mut stored = StoredEffect::fresh();
        stored.open = vec![
            OpenNode {
                node: nid("a"),
                attempt: 1,
            },
            OpenNode {
                node: nid("b"),
                attempt: 3,
            },
        ];
        let drafts = derive_node_runs(&update, &stored).unwrap();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().all(|d| d.state == LoggedState::Cancelled));
        assert_eq!(drafts[1].attempt, 3);
        assert!(derive(&update).is_empty(), "no open node, no event");
    }

    #[test]
    fn a_replayed_commit_derives_nothing() {
        let update = CaseUpdate::new()
            .with_attempt(attempt(AttemptState::Done))
            .with_transition(complete_step(Completion::Done { output: None }, 90))
            .with_at(at(90));
        let mut stored = StoredEffect::fresh();
        stored.kept_earlier_attempt = true;
        stored.transition_applied = false;
        assert!(derive_node_runs(&update, &stored).unwrap().is_empty());
    }

    #[test]
    fn drafts_are_unique_per_node_attempt_and_state() {
        let update = CaseUpdate::new()
            .with_attempt(attempt(AttemptState::Done))
            .with_transition(complete_step(Completion::Done { output: None }, 91))
            .with_at(at(91));
        assert_eq!(derive(&update).len(), 1);
    }

    #[test]
    fn an_update_without_an_instant_is_refused() {
        let with_attempt = CaseUpdate::new().with_attempt(attempt(AttemptState::Started));
        let error = derive_node_runs(&with_attempt, &StoredEffect::fresh()).unwrap_err();
        assert_eq!(error.code(), "bpm_engine.store_failed");
        let with_timer = CaseUpdate::new().with_timer(TimerRecord {
            node: nid("n"),
            attempt: 1,
            due: Some(at(1)),
            event: None,
            human: None,
        });
        assert!(derive_node_runs(&with_timer, &StoredEffect::fresh()).is_err());
        assert!(derive(&CaseUpdate::new()).is_empty());
    }

    #[test]
    fn consumer_names_are_validated() {
        assert!(validate_consumer("insight.v1-a_b").is_ok());
        for bad in ["", "Upper", "has space", "semi;colon", &"a".repeat(65)] {
            assert!(validate_consumer(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens_round_trip() {
        for state in [
            LoggedState::Started,
            LoggedState::Waiting,
            LoggedState::Done,
            LoggedState::Failed,
            LoggedState::Cancelled,
        ] {
            assert_eq!(LoggedState::from_token(state.token()), Some(state));
        }
        for wait in [
            LoggedWait::Timer,
            LoggedWait::Event,
            LoggedWait::HumanTask,
            LoggedWait::IntegrationRetry,
        ] {
            assert_eq!(LoggedWait::from_token(wait.token()), Some(wait));
        }
        assert_eq!(LoggedState::from_token("x"), None);
    }
}
