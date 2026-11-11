//! The streaming fold: [`Accumulator`] turns a node-run event feed into an [`Insight`].
//!
//! The fold is incremental and order-preserving, so a backfill that pushes a stored feed and a
//! live feed that pushes the same events as they happen give **exactly** the same result (the
//! property test proves it): push a chunk, keep going, and finish once. Only the per-step
//! counters, the per-step completed-attempt cycle times and the currently open attempts are
//! retained; a finished attempt leaves only its numbers behind.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_dag::NodeId;

use crate::access::ProcessAccess;
use crate::bottleneck::{Bottleneck, BottleneckPolicy, bottlenecks};
use crate::duration::Millis;
use crate::event::{NodeRunEvent, RunState, WaitReason};
use crate::percentile::percentile;
use crate::process::{ProcessKey, StepKey};
use crate::stats::StepStats;
use crate::stuck::{StuckCase, StuckPolicy, stuck_cases};

/// The key of one open attempt: the process version, the run and the node.
///
/// The engine runs at most one attempt of a node at a time, so this key identifies the open
/// attempt uniquely; the attempt number is carried inside the track.
type OpenKey = (ProcessKey, CaseId, NodeId);

/// What closed an attempt, for the counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseKind {
    /// The attempt succeeded.
    Done,
    /// The attempt failed terminally.
    Failed,
    /// The attempt failed and was retried.
    Retried,
}

/// The state of one open attempt, kept only until it finishes.
#[derive(Clone, Debug)]
struct Track {
    attempt: u32,
    kind: NodeKind,
    owner: Option<String>,
    started_at: OffsetDateTime,
    segment_start: OffsetDateTime,
    wait_start: Option<OffsetDateTime>,
    state: RunState,
    wait: Option<WaitReason>,
    work: Millis,
    wait_time: Millis,
}

impl Track {
    fn fresh(event: &NodeRunEvent) -> Self {
        Self {
            attempt: event.attempt,
            kind: event.kind.clone(),
            owner: event.owner.clone(),
            started_at: event.at,
            segment_start: event.at,
            wait_start: None,
            state: RunState::Started,
            wait: None,
            work: Millis::ZERO,
            wait_time: Millis::ZERO,
        }
    }
}

/// The running aggregate of one step, folded from events and finished into a [`StepStats`].
#[derive(Clone, Debug)]
struct StepAgg {
    kind: NodeKind,
    attempts: u64,
    done: u64,
    failed: u64,
    retries: u64,
    completed: u64,
    work: Millis,
    wait: Millis,
    cycles: Vec<i64>,
    queue_entered: u64,
    queue_left: u64,
    waiting: u64,
    max_waiting: u64,
}

impl StepAgg {
    fn new(kind: &NodeKind) -> Self {
        Self {
            kind: kind.clone(),
            attempts: 0,
            done: 0,
            failed: 0,
            retries: 0,
            completed: 0,
            work: Millis::ZERO,
            wait: Millis::ZERO,
            cycles: Vec::new(),
            queue_entered: 0,
            queue_left: 0,
            waiting: 0,
            max_waiting: 0,
        }
    }

    fn finish(mut self, process: ProcessKey, node: NodeId) -> StepStats {
        self.cycles.sort_unstable();
        let cycles = &self.cycles;
        let as_millis = |value: Option<i64>| value.map(Millis::from_millis);
        StepStats {
            process,
            node,
            kind: self.kind,
            attempts: self.attempts,
            completed: self.completed,
            done: self.done,
            failed: self.failed,
            retries: self.retries,
            total: self.work + self.wait,
            work: self.work,
            wait: self.wait,
            min: as_millis(cycles.first().copied()),
            p50: as_millis(percentile(cycles, 50)),
            p90: as_millis(percentile(cycles, 90)),
            p99: as_millis(percentile(cycles, 99)),
            max: as_millis(cycles.last().copied()),
            queue_entered: self.queue_entered,
            queue_left: self.queue_left,
            queue_growth: i64::try_from(self.queue_entered).unwrap_or(i64::MAX)
                - i64::try_from(self.queue_left).unwrap_or(i64::MAX),
            max_waiting: self.max_waiting,
        }
    }
}

/// One attempt that has not finished: what the stuck queue is built from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAttempt {
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
    /// Where the attempt is: running or waiting.
    pub state: RunState,
    /// Why it is waiting, when it is.
    pub wait: Option<WaitReason>,
    /// When the attempt entered its current state.
    #[serde(with = "crate::wire::instant")]
    pub since: OffsetDateTime,
    /// When the attempt first started.
    #[serde(with = "crate::wire::instant")]
    pub started_at: OffsetDateTime,
}

impl OpenAttempt {
    fn from_track(key: OpenKey, track: &Track) -> Self {
        let (process, case, node) = key;
        let since = if track.state == RunState::Waiting {
            track.wait_start.unwrap_or(track.segment_start)
        } else {
            track.segment_start
        };
        Self {
            process,
            case,
            node,
            attempt: track.attempt,
            kind: track.kind.clone(),
            owner: track.owner.clone(),
            state: track.state,
            wait: track.wait,
            since,
            started_at: track.started_at,
        }
    }
}

/// The finished result of a feed: the per-step statistics and the attempts still open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Insight {
    steps: Vec<StepStats>,
    open: Vec<OpenAttempt>,
}

impl Insight {
    /// The per-process-version-and-node statistics, sorted by process version then node.
    #[must_use]
    pub fn steps(&self) -> &[StepStats] {
        &self.steps
    }

    /// The attempts still open at the end of the feed.
    #[must_use]
    pub fn open_attempts(&self) -> &[OpenAttempt] {
        &self.open
    }

    /// Whether the feed held no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.open.is_empty()
    }

    /// The step statistics, consumed.
    #[must_use]
    pub fn into_steps(self) -> Vec<StepStats> {
        self.steps
    }

    /// The bottlenecks of every process in the feed, ranked by contribution to cycle time and
    /// then by queue growth (see [`bottlenecks`]).
    #[must_use]
    pub fn bottlenecks(&self, policy: &BottleneckPolicy) -> Vec<Bottleneck> {
        bottlenecks(&self.steps, policy)
    }

    /// The runs not progressing past a per-node expected duration (see [`stuck_cases`]).
    #[must_use]
    pub fn stuck_cases(&self, now: OffsetDateTime, policy: &StuckPolicy) -> Vec<StuckCase> {
        stuck_cases(&self.open, now, policy)
    }

    /// The same insight seen through a viewer's process-level read permission.
    ///
    /// A process the viewer may not read is **absent**, not a row of zeros.
    #[must_use]
    pub fn visible_to(&self, access: &dyn ProcessAccess) -> Self {
        Self {
            steps: self
                .steps
                .iter()
                .filter(|step| access.may_read(&step.process))
                .cloned()
                .collect(),
            open: self
                .open
                .iter()
                .filter(|attempt| access.may_read(&attempt.process))
                .cloned()
                .collect(),
        }
    }
}

/// The incremental fold of a node-run event feed.
#[derive(Clone, Debug, Default)]
pub struct Accumulator {
    steps: BTreeMap<StepKey, StepAgg>,
    open: BTreeMap<OpenKey, Track>,
}

impl Accumulator {
    /// An empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no event has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.open.is_empty()
    }

    /// Folds one event.
    pub fn push(&mut self, event: NodeRunEvent) {
        let open_key: OpenKey = (
            event.process.clone(),
            event.case.clone(),
            event.node.clone(),
        );
        match event.state {
            RunState::Started => self.on_started(open_key, event),
            RunState::Waiting => self.on_waiting(open_key, event),
            RunState::Done | RunState::Failed => self.on_terminal(open_key, event),
        }
    }

    /// Folds one event when the viewer may read its process; an event of a process the viewer
    /// may not read is dropped, so the process never appears in the result.
    pub fn push_for(&mut self, event: NodeRunEvent, access: &dyn ProcessAccess) {
        if access.may_read(&event.process) {
            self.push(event);
        }
    }

    /// Folds a whole feed.
    pub fn push_all(&mut self, events: impl IntoIterator<Item = NodeRunEvent>) {
        for event in events {
            self.push(event);
        }
    }

    /// The runs not progressing past a per-node expected duration, from the attempts still open.
    #[must_use]
    pub fn stuck_cases(&self, now: OffsetDateTime, policy: &StuckPolicy) -> Vec<StuckCase> {
        let open: Vec<OpenAttempt> = self
            .open
            .iter()
            .map(|(key, track)| OpenAttempt::from_track(key.clone(), track))
            .collect();
        stuck_cases(&open, now, policy)
    }

    /// Finishes the fold: the step statistics and the attempts still open.
    #[must_use]
    pub fn finish(self) -> Insight {
        let mut steps: Vec<StepStats> = self
            .steps
            .into_iter()
            .map(|(key, agg)| agg.finish(key.process, key.node))
            .collect();
        steps.sort_by(|a, b| a.process.cmp(&b.process).then_with(|| a.node.cmp(&b.node)));
        let open: Vec<OpenAttempt> = self
            .open
            .iter()
            .map(|(key, track)| OpenAttempt::from_track(key.clone(), track))
            .collect();
        Insight { steps, open }
    }

    fn step_mut(&mut self, process: &ProcessKey, node: &NodeId, kind: &NodeKind) -> &mut StepAgg {
        let key = StepKey::new(process.clone(), node.clone());
        self.steps.entry(key).or_insert_with(|| StepAgg::new(kind))
    }

    fn on_started(&mut self, open_key: OpenKey, event: NodeRunEvent) {
        if let Some(track) = self.open.remove(&open_key) {
            if track.attempt == event.attempt {
                self.resume(open_key, track, &event);
                return;
            }
            // A new attempt of the same node: the previous one is finished (retried or replayed).
            self.close(&open_key, track, event.at, CloseKind::Retried);
        }
        let step = self.step_mut(&event.process, &event.node, &event.kind);
        step.attempts += 1;
        let track = Track::fresh(&event);
        self.open.insert(open_key, track);
    }

    fn resume(&mut self, open_key: OpenKey, mut track: Track, event: &NodeRunEvent) {
        if track.state == RunState::Waiting {
            let since = track.wait_start.unwrap_or(event.at);
            track.wait_time += Millis::between(since, event.at);
            track.wait_start = None;
            let step = self.step_mut(&event.process, &event.node, &event.kind);
            step.queue_left += 1;
            step.waiting = step.waiting.saturating_sub(1);
        }
        track.segment_start = event.at;
        track.state = RunState::Started;
        track.wait = None;
        if event.owner.is_some() {
            track.owner = event.owner.clone();
        }
        self.open.insert(open_key, track);
    }

    fn on_waiting(&mut self, open_key: OpenKey, event: NodeRunEvent) {
        let mut track = match self.open.remove(&open_key) {
            Some(track) => track,
            None => {
                // A wait before its start event was seen: count the attempt and wait from here.
                let step = self.step_mut(&event.process, &event.node, &event.kind);
                step.attempts += 1;
                Track::fresh(&event)
            }
        };
        if track.state == RunState::Started {
            track.work += Millis::between(track.segment_start, event.at);
        } else if let Some(since) = track.wait_start {
            // Already waiting: the previous wait interval ends here.
            track.wait_time += Millis::between(since, event.at);
        }
        track.state = RunState::Waiting;
        track.wait_start = Some(event.at);
        track.wait = event.wait;
        track.kind = event.kind.clone();
        if event.owner.is_some() {
            track.owner = event.owner.clone();
        }
        let step = self.step_mut(&event.process, &event.node, &event.kind);
        step.queue_entered += 1;
        step.waiting += 1;
        step.max_waiting = step.max_waiting.max(step.waiting);
        self.open.insert(open_key, track);
    }

    fn on_terminal(&mut self, open_key: OpenKey, event: NodeRunEvent) {
        let kind = if event.state == RunState::Done {
            CloseKind::Done
        } else if event.retry {
            CloseKind::Retried
        } else {
            CloseKind::Failed
        };
        if let Some(track) = self.open.remove(&open_key) {
            self.close(&open_key, track, event.at, kind);
            return;
        }
        // A terminal event with no start: a zero-length attempt.
        let step = self.step_mut(&event.process, &event.node, &event.kind);
        step.attempts += 1;
        step.completed += 1;
        step.cycles.push(0);
        match kind {
            CloseKind::Done => step.done += 1,
            CloseKind::Failed => step.failed += 1,
            CloseKind::Retried => step.retries += 1,
        }
    }

    fn close(&mut self, open_key: &OpenKey, mut track: Track, at: OffsetDateTime, kind: CloseKind) {
        let was_waiting = track.state == RunState::Waiting;
        if was_waiting {
            let since = track.wait_start.unwrap_or(at);
            track.wait_time += Millis::between(since, at);
        } else {
            track.work += Millis::between(track.segment_start, at);
        }
        let cycle = track.work + track.wait_time;
        let (process, _, node) = open_key;
        let step = self.step_mut(process, node, &track.kind);
        if was_waiting {
            step.queue_left += 1;
            step.waiting = step.waiting.saturating_sub(1);
        }
        step.completed += 1;
        step.work += track.work;
        step.wait += track.wait_time;
        step.cycles.push(cycle.as_millis());
        match kind {
            CloseKind::Done => step.done += 1,
            CloseKind::Failed => step.failed += 1,
            CloseKind::Retried => step.retries += 1,
        }
    }
}

/// Aggregates a whole feed in one call.
#[must_use]
pub fn aggregate(events: impl IntoIterator<Item = NodeRunEvent>) -> Insight {
    let mut accumulator = Accumulator::new();
    accumulator.push_all(events);
    accumulator.finish()
}

/// Aggregates a whole feed for a viewer, dropping every event of a process the viewer may not
/// read.
#[must_use]
pub fn aggregate_for(
    events: impl IntoIterator<Item = NodeRunEvent>,
    access: &dyn ProcessAccess,
) -> Insight {
    let mut accumulator = Accumulator::new();
    for event in events {
        accumulator.push_for(event, access);
    }
    accumulator.finish()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event::AttemptRef;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
    }

    fn attempt(node: &str, attempt: u32) -> AttemptRef {
        AttemptRef::new(
            ProcessKey::new("order_flow", 1),
            CaseId::new("case1").unwrap(),
            NodeId::new(node).unwrap(),
            attempt,
            NodeKind::Decision,
        )
    }

    #[test]
    fn a_wait_splits_cycle_time_into_work_and_wait() {
        let events = vec![
            NodeRunEvent::started(attempt("approve", 1), at(0)),
            NodeRunEvent::waiting(attempt("approve", 1), at(10), WaitReason::HumanTask),
            NodeRunEvent::started(attempt("approve", 1), at(40)),
            NodeRunEvent::done(attempt("approve", 1), at(50)),
        ];
        let insight = aggregate(events);
        let step = &insight.steps()[0];
        assert_eq!(step.work, Millis::from_seconds(20));
        assert_eq!(step.wait, Millis::from_seconds(30));
        assert_eq!(step.total, Millis::from_seconds(50));
        assert_eq!(step.p50, Some(Millis::from_seconds(50)));
        assert_eq!(step.queue_entered, 1);
        assert_eq!(step.queue_left, 1);
        assert_eq!(step.queue_growth, 0);
    }

    #[test]
    fn a_retried_attempt_is_counted_and_its_replacement_starts_fresh() {
        let events = vec![
            NodeRunEvent::started(attempt("call", 1), at(0)),
            NodeRunEvent::retrying(attempt("call", 1), at(5), "conn.timeout"),
            NodeRunEvent::started(attempt("call", 2), at(15)),
            NodeRunEvent::done(attempt("call", 2), at(20)),
        ];
        let insight = aggregate(events);
        let step = &insight.steps()[0];
        assert_eq!(step.attempts, 2);
        assert_eq!(step.completed, 2);
        assert_eq!(step.done, 1);
        assert_eq!(step.retries, 1);
        assert_eq!(step.retry_rate().per_mille(), 500);
        // Attempt 1: 5 s work + 10 s retry wait; attempt 2: 5 s work.
        assert_eq!(step.total, Millis::from_seconds(20));
    }

    #[test]
    fn chunked_and_whole_folds_agree() {
        let events = vec![
            NodeRunEvent::started(attempt("approve", 1), at(0)),
            NodeRunEvent::waiting(attempt("approve", 1), at(10), WaitReason::HumanTask),
            NodeRunEvent::started(attempt("approve", 1), at(40)),
            NodeRunEvent::done(attempt("approve", 1), at(50)),
        ];
        let whole = aggregate(events.clone());
        let mut accumulator = Accumulator::new();
        accumulator.push_all(events[..2].iter().cloned());
        accumulator.push_all(events[2..].iter().cloned());
        assert_eq!(accumulator.finish(), whole);
    }
}
