//! The accumulator's stored form: a versioned, serialisable checkpoint (WP-BPM-11.T7).
//!
//! A host that resumes after a restart needs the folded state, not only a cursor into the feed.
//! [`Accumulator::export_state`] returns an [`AccumulatorState`] (integers and strings only, no
//! float, `deny_unknown_fields`) and [`Accumulator::from_state`] rebuilds the accumulator from
//! it. The law the property tests prove: folding `a ++ b` equals restoring the exported state of
//! `fold(a)` and folding `b`, wherever the cut falls, including between a `started` and its
//! `done`.
//!
//! Loading is fail-closed. An unknown [`STATE_VERSION`] is refused with
//! `bpm_insight.state_version` (never guessed at) and any inconsistency, such as counts that do
//! not add up, an open attempt without its step, a duplicate key or an oversized window, with
//! `bpm_insight.state_corrupt`. Nothing here panics on bad input.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_dag::NodeId;

use crate::accumulator::{Accumulator, ENDED_WINDOW, Ended, OpenKey, StepAgg, Track};
use crate::duration::Millis;
use crate::error::InsightError;
use crate::event::{RunState, WaitReason};
use crate::histogram::{Histogram, HistogramState};
use crate::process::{ProcessKey, StepKey};

/// The version [`Accumulator::export_state`] writes and [`Accumulator::from_state`] accepts.
pub const STATE_VERSION: u32 = 1;

/// The stored form of one step's running aggregate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepState {
    /// The process version.
    pub process: ProcessKey,
    /// The node.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The node's execution kind.
    pub kind: NodeKind,
    /// Attempts seen.
    pub attempts: u64,
    /// Attempts that succeeded.
    pub done: u64,
    /// Attempts that failed terminally.
    pub failed: u64,
    /// Attempts that failed and were retried.
    pub retries: u64,
    /// Attempts that ended (done, failed or retried).
    pub completed: u64,
    /// Total work time of completed attempts.
    pub work: Millis,
    /// Total wait time of completed attempts.
    pub wait: Millis,
    /// The completed attempts' cycle times.
    pub cycles: HistogramState,
    /// Times an attempt entered a wait.
    pub queue_entered: u64,
    /// Times an attempt left a wait.
    pub queue_left: u64,
    /// Attempts waiting now.
    pub waiting: u64,
    /// The most attempts waiting at once.
    pub max_waiting: u64,
}

/// The stored form of one open attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenState {
    /// The process version.
    pub process: ProcessKey,
    /// The run (the Case).
    pub case: CaseId,
    /// The node.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The attempt number.
    pub attempt: u32,
    /// The node's execution kind.
    pub kind: NodeKind,
    /// Who owns the waiting work, when someone does.
    pub owner: Option<String>,
    /// When the attempt first started.
    #[serde(with = "crate::wire::instant")]
    pub started_at: OffsetDateTime,
    /// When the current running segment or wait began its accounting.
    #[serde(with = "crate::wire::instant")]
    pub segment_start: OffsetDateTime,
    /// When the current wait began, while waiting.
    #[serde(with = "crate::wire::option_instant")]
    pub wait_start: Option<OffsetDateTime>,
    /// Running or waiting.
    pub state: RunState,
    /// Why the attempt waits, while it does.
    pub wait_reason: Option<WaitReason>,
    /// Work time accumulated so far.
    pub work: Millis,
    /// Wait time accumulated so far.
    pub wait_time: Millis,
}

/// The stored form of one recently ended attempt (the duplicate-protection window).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndedState {
    /// The process version.
    pub process: ProcessKey,
    /// The run (the Case).
    pub case: CaseId,
    /// The node.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The attempt number that ended.
    pub attempt: u32,
    /// When it ended.
    #[serde(with = "crate::wire::instant")]
    pub at: OffsetDateTime,
}

/// The versioned checkpoint of an [`Accumulator`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccumulatorState {
    /// The state format version; see [`STATE_VERSION`].
    pub version: u32,
    /// Per-step aggregates, ascending by process version then node.
    pub steps: Vec<StepState>,
    /// Open attempts, ascending by process version, run, then node.
    pub open: Vec<OpenState>,
    /// The most recently ended attempts, oldest first, at most 256.
    pub ended: Vec<EndedState>,
}

fn corrupt(detail: impl Into<String>) -> InsightError {
    InsightError::StateCorrupt {
        detail: detail.into(),
    }
}

impl Accumulator {
    /// The versioned checkpoint of the fold so far, for the host to store beside its cursor.
    ///
    /// Deterministic: the same fold always exports the same state.
    #[must_use]
    pub fn export_state(&self) -> AccumulatorState {
        let steps = self
            .steps
            .iter()
            .map(|(key, agg)| StepState {
                process: key.process.clone(),
                node: key.node.clone(),
                kind: agg.kind.clone(),
                attempts: agg.attempts,
                done: agg.done,
                failed: agg.failed,
                retries: agg.retries,
                completed: agg.completed,
                work: agg.work,
                wait: agg.wait,
                cycles: agg.cycles.to_state(),
                queue_entered: agg.queue_entered,
                queue_left: agg.queue_left,
                waiting: agg.waiting,
                max_waiting: agg.max_waiting,
            })
            .collect();
        let open = self
            .open
            .iter()
            .map(|((process, case, node), track)| OpenState {
                process: process.clone(),
                case: case.clone(),
                node: node.clone(),
                attempt: track.attempt,
                kind: track.kind.clone(),
                owner: track.owner.clone(),
                started_at: track.started_at,
                segment_start: track.segment_start,
                wait_start: track.wait_start,
                state: track.state,
                wait_reason: track.wait,
                work: track.work,
                wait_time: track.wait_time,
            })
            .collect();
        let ended = self
            .ended_order
            .values()
            .filter_map(|key| {
                let entry = self.ended.get(key)?;
                let (process, case, node) = key;
                Some(EndedState {
                    process: process.clone(),
                    case: case.clone(),
                    node: node.clone(),
                    attempt: entry.attempt,
                    at: entry.at,
                })
            })
            .collect();
        AccumulatorState {
            version: STATE_VERSION,
            steps,
            open,
            ended,
        }
    }

    /// Rebuilds an accumulator from a stored checkpoint.
    ///
    /// # Errors
    ///
    /// [`InsightError::StateVersion`] for a version this build does not know, and
    /// [`InsightError::StateCorrupt`] for any inconsistent content. The state is never partly
    /// loaded.
    pub fn from_state(state: AccumulatorState) -> Result<Self, InsightError> {
        if state.version != STATE_VERSION {
            return Err(InsightError::StateVersion {
                found: state.version,
                supported: STATE_VERSION,
            });
        }
        let mut accumulator = Self::new();
        for step in state.steps {
            let StepState {
                process,
                node,
                kind,
                attempts,
                done,
                failed,
                retries,
                completed,
                work,
                wait,
                cycles,
                queue_entered,
                queue_left,
                waiting,
                max_waiting,
            } = step;
            let cycles = Histogram::from_state(cycles)?;
            let outcomes = done
                .checked_add(failed)
                .and_then(|n| n.checked_add(retries));
            if cycles.count() != completed || outcomes != Some(completed) {
                return Err(corrupt("a step's outcome counts do not add up"));
            }
            if completed > attempts || queue_left > queue_entered || waiting > max_waiting {
                return Err(corrupt("a step's counters are inconsistent"));
            }
            let agg = StepAgg {
                kind,
                attempts,
                done,
                failed,
                retries,
                completed,
                work,
                wait,
                cycles,
                queue_entered,
                queue_left,
                waiting,
                max_waiting,
            };
            if accumulator
                .steps
                .insert(StepKey::new(process, node), agg)
                .is_some()
            {
                return Err(corrupt("a step appears twice"));
            }
        }
        for open in state.open {
            let OpenState {
                process,
                case,
                node,
                attempt,
                kind,
                owner,
                started_at,
                segment_start,
                wait_start,
                state: run_state,
                wait_reason,
                work,
                wait_time,
            } = open;
            let coherent = match run_state {
                RunState::Started => wait_start.is_none() && wait_reason.is_none(),
                RunState::Waiting => wait_start.is_some(),
                RunState::Done | RunState::Failed => false,
            };
            if !coherent {
                return Err(corrupt("an open attempt has an impossible state"));
            }
            let step_key = StepKey::new(process.clone(), node.clone());
            if !accumulator.steps.contains_key(&step_key) {
                return Err(corrupt("an open attempt has no step"));
            }
            let track = Track {
                attempt,
                kind,
                owner,
                started_at,
                segment_start,
                wait_start,
                state: run_state,
                wait: wait_reason,
                work,
                wait_time,
            };
            if accumulator
                .open
                .insert((process, case, node), track)
                .is_some()
            {
                return Err(corrupt("an open attempt appears twice"));
            }
        }
        if state.ended.len() > ENDED_WINDOW {
            return Err(corrupt("the ended-attempt window is too large"));
        }
        for ended in state.ended {
            let key: OpenKey = (ended.process, ended.case, ended.node);
            let seq = accumulator.next_seq;
            accumulator.next_seq += 1;
            let entry = Ended {
                attempt: ended.attempt,
                at: ended.at,
                seq,
            };
            if accumulator.ended.insert(key.clone(), entry).is_some() {
                return Err(corrupt("an ended attempt appears twice"));
            }
            accumulator.ended_order.insert(seq, key);
        }
        Ok(accumulator)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event::{AttemptRef, NodeRunEvent};
    use spot_bpm_engine::NodeKind;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
    }

    fn attempt(case: &str, n: u32) -> AttemptRef {
        AttemptRef::new(
            ProcessKey::new("order_flow", 1),
            CaseId::new(case).unwrap(),
            NodeId::new("approve").unwrap(),
            n,
            NodeKind::Decision,
        )
    }

    fn sample() -> Accumulator {
        let mut acc = Accumulator::new();
        acc.push_all([
            NodeRunEvent::started(attempt("c1", 1), at(0)),
            NodeRunEvent::waiting(attempt("c1", 1), at(5), WaitReason::HumanTask),
            NodeRunEvent::started(attempt("c1", 1), at(9)),
            NodeRunEvent::done(attempt("c1", 1), at(12)),
            NodeRunEvent::started(attempt("c2", 1), at(13)),
            NodeRunEvent::waiting(attempt("c2", 1), at(14), WaitReason::Timer),
        ]);
        acc
    }

    #[test]
    fn state_round_trips_through_json() {
        let acc = sample();
        let state = acc.export_state();
        assert_eq!(state.version, STATE_VERSION);
        let json = serde_json::to_string(&state).unwrap();
        let back: AccumulatorState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
        let restored = Accumulator::from_state(back).unwrap();
        assert_eq!(restored.export_state(), state);
        assert_eq!(restored.finish(), acc.finish());
    }

    #[test]
    fn an_unknown_version_is_refused_with_a_stable_code() {
        let mut state = sample().export_state();
        state.version = 2;
        let err = Accumulator::from_state(state).unwrap_err();
        assert_eq!(err.code(), "bpm_insight.state_version");
        let mut state = sample().export_state();
        state.version = 0;
        assert_eq!(
            Accumulator::from_state(state).unwrap_err().code(),
            "bpm_insight.state_version"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json = serde_json::to_string(&sample().export_state()).unwrap();
        let tampered = json.replacen("{", "{\"extra\":1,", 1);
        assert!(serde_json::from_str::<AccumulatorState>(&tampered).is_err());
    }

    #[test]
    fn corrupt_content_is_refused_never_panics() {
        let good = sample().export_state();

        let mut bad = good.clone();
        bad.steps[0].completed += 1;
        assert_eq!(
            Accumulator::from_state(bad).unwrap_err().code(),
            "bpm_insight.state_corrupt"
        );

        let mut bad = good.clone();
        bad.steps.clear();
        assert_eq!(
            Accumulator::from_state(bad).unwrap_err().code(),
            "bpm_insight.state_corrupt"
        );

        let mut bad = good.clone();
        bad.steps.push(bad.steps[0].clone());
        assert!(Accumulator::from_state(bad).is_err());

        let mut bad = good.clone();
        bad.open.push(bad.open[0].clone());
        assert!(Accumulator::from_state(bad).is_err());

        let mut bad = good.clone();
        bad.open[0].state = RunState::Done;
        assert!(Accumulator::from_state(bad).is_err());

        let mut bad = good.clone();
        bad.ended.push(bad.ended[0].clone());
        assert!(Accumulator::from_state(bad).is_err());

        let mut bad = good.clone();
        bad.ended = (0..=ENDED_WINDOW)
            .map(|i| {
                let mut e = good.ended[0].clone();
                e.case = CaseId::new(&format!("x{i}")).unwrap();
                e
            })
            .collect();
        assert!(Accumulator::from_state(bad).is_err());

        let mut bad = good;
        bad.steps[0].cycles.buckets.push((9999, 1));
        assert!(Accumulator::from_state(bad).is_err());
    }
}
