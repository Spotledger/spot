//! SLA policy, business-calendar awareness and the timer port the SLA runs on.
//!
//! An SLA is a per-node policy: reminder offsets, an escalation chain (to a manager, to a role or
//! to a user), and a final action (auto-approve, auto-reject, escalate again, or nothing). The
//! policy is carried on the [`Task`](crate::Task) itself and its steps are timers registered with
//! a [`SlaTimerPort`], so they survive a restart. [`InMemorySlaTimerPort`] is the reference; the
//! [`EngineSlaTimers`] adapter wires the port to the engine's own
//! [`TimerPort`](spot_bpm_engine::TimerPort), which is the host's route to `spot-scheduler`.
//!
//! Time is always an argument: nothing here reads a clock. The [`BusinessCalendar`] decides what
//! "after n seconds" means, and the default is the wall clock.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, TimerPort, TimerRegistration};
use spot_dag::NodeId;

use crate::task::TaskId;

/// What a reminder or escalation notifies, relative to the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    /// The task's current assignees.
    Assignees,
    /// The named user.
    User(String),
    /// The members of the named role.
    Role(String),
    /// The manager of each assignee.
    Manager,
}

/// One reminder: notify these recipients this many seconds after the task opened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    /// Seconds after the task opened.
    pub after_seconds: u64,
    /// Who to notify.
    pub to: Vec<Recipient>,
}

/// Where an escalation sends the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationTarget {
    /// The assignee's manager.
    Manager,
    /// The members of a role (the first member acts).
    Role(String),
    /// A named user.
    User(String),
}

/// One escalation: this many seconds after the task opened, send it to this target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Escalation {
    /// Seconds after the task opened.
    pub after_seconds: u64,
    /// Where the task goes.
    pub to: EscalationTarget,
    /// Who to notify.
    pub notify: Vec<Recipient>,
}

/// What the SLA does at its final deadline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalAction {
    /// Do nothing at the deadline.
    #[default]
    None,
    /// Complete the task as an approval by the clock (`system:sla`, flagged automatic). Only for
    /// a node that declares `sla_auto_approve` and is not segregated.
    AutoApprove,
    /// Complete the task as a rejection, as the system principal.
    AutoReject,
    /// Escalate once more to the last escalation's target.
    EscalateAgain,
}

/// A per-node SLA policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlaPolicy {
    /// The reminder offsets.
    #[serde(default)]
    pub reminders: Vec<Reminder>,
    /// The escalation chain.
    #[serde(default)]
    pub escalations: Vec<Escalation>,
    /// What happens at the final deadline.
    #[serde(default)]
    pub final_action: FinalAction,
    /// The final deadline, in seconds after the task opened; required for the final action.
    #[serde(default)]
    pub final_after_seconds: Option<u64>,
    /// Whether the node declares `sla_auto_approve = true`. Without it an `auto-approve` final
    /// action is refused when the task opens, and never runs.
    #[serde(default)]
    pub auto_approve_declared: bool,
    /// The policy's id for the audit trail; the node id is used when absent.
    #[serde(default)]
    pub id: Option<String>,
}

/// The kind of one SLA step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlaStepKind {
    /// A reminder, by index into the policy's reminders.
    Reminder(usize),
    /// An escalation, by index into the policy's escalations.
    Escalation(usize),
    /// The final action.
    Final,
}

/// One step of an SLA: its kind and how far after the task opened it fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlaStep {
    /// What the step does.
    pub kind: SlaStepKind,
    /// Seconds after the task opened.
    pub after_seconds: u64,
}

impl SlaPolicy {
    /// The steps, reminders and escalations merged and sorted by offset, then the final action.
    ///
    /// The sort is stable, so a reminder and an escalation at the same offset keep their declared
    /// order (reminders before escalations).
    #[must_use]
    pub fn steps(&self) -> Vec<SlaStep> {
        let mut steps: Vec<SlaStep> = Vec::new();
        for (index, reminder) in self.reminders.iter().enumerate() {
            steps.push(SlaStep {
                kind: SlaStepKind::Reminder(index),
                after_seconds: reminder.after_seconds,
            });
        }
        for (index, escalation) in self.escalations.iter().enumerate() {
            steps.push(SlaStep {
                kind: SlaStepKind::Escalation(index),
                after_seconds: escalation.after_seconds,
            });
        }
        steps.sort_by_key(|step| step.after_seconds);
        if self.final_action != FinalAction::None
            && let Some(after) = self.final_after_seconds
        {
            steps.push(SlaStep {
                kind: SlaStepKind::Final,
                after_seconds: after,
            });
        }
        steps
    }

    /// The step at `index`, when the policy has one.
    #[must_use]
    pub fn step(&self, index: usize) -> Option<SlaStep> {
        self.steps().get(index).copied()
    }

    /// Whether the policy has any step at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps().is_empty()
    }
}

/// What "n seconds after an instant" means on this calendar.
pub trait BusinessCalendar: Send + Sync {
    /// The instant `seconds` after `start`.
    fn instant_after(&self, start: OffsetDateTime, seconds: u64) -> OffsetDateTime;
}

/// The default calendar: the wall clock, where a second is a second.
#[derive(Debug, Clone, Copy, Default)]
pub struct WallClockCalendar;

impl BusinessCalendar for WallClockCalendar {
    fn instant_after(&self, start: OffsetDateTime, seconds: u64) -> OffsetDateTime {
        start + Duration::from_secs(seconds)
    }
}

/// A calendar that skips weekends: each whole day of the offset advances to the next weekday.
#[derive(Debug, Clone, Copy, Default)]
pub struct BusinessDaysCalendar;

impl BusinessCalendar for BusinessDaysCalendar {
    fn instant_after(&self, start: OffsetDateTime, seconds: u64) -> OffsetDateTime {
        let day = 86_400;
        let mut instant = start;
        let mut remaining = seconds;
        while remaining > 0 {
            instant += Duration::from_secs(remaining.min(day));
            remaining = remaining.saturating_sub(day);
            while is_weekend(instant) {
                instant += Duration::from_secs(day);
            }
        }
        instant
    }
}

/// Whether `instant` falls on a Saturday or Sunday.
fn is_weekend(instant: OffsetDateTime) -> bool {
    instant.date().weekday().number_days_from_monday() >= 5
}

/// One registered SLA timer: the task, its Case and node, the step index and the due instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlaTimer {
    /// The task the timer belongs to.
    pub task: TaskId,
    /// The Case, as text.
    pub case: String,
    /// The Decision node, as text.
    pub node: String,
    /// The index of the step in the policy's [`SlaPolicy::steps`].
    pub step: usize,
    /// When it fires.
    pub due: OffsetDateTime,
}

/// Where SLA timers are registered so they survive a restart.
pub trait SlaTimerPort: Send + Sync {
    /// Registers a timer, replacing any pending timer for the same task.
    fn register(&self, timer: SlaTimer);

    /// Clears a task's pending timer, once it fired or the task ended.
    fn clear(&self, task: &TaskId);

    /// The timers due at `now`, each returned at most once until re-registered.
    fn due(&self, now: OffsetDateTime) -> Vec<SlaTimer>;
}

/// One entry of the in-memory port: the timer and whether it has fired.
#[derive(Debug, Clone)]
struct Entry {
    timer: SlaTimer,
    fired: bool,
}

/// The in-memory reference [`SlaTimerPort`].
#[derive(Debug, Clone, Default)]
pub struct InMemorySlaTimerPort {
    entries: Arc<Mutex<BTreeMap<TaskId, Entry>>>,
}

impl InMemorySlaTimerPort {
    /// An empty port.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<TaskId, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SlaTimerPort for InMemorySlaTimerPort {
    fn register(&self, timer: SlaTimer) {
        self.lock().insert(
            timer.task.clone(),
            Entry {
                timer,
                fired: false,
            },
        );
    }

    fn clear(&self, task: &TaskId) {
        self.lock().remove(task);
    }

    fn due(&self, now: OffsetDateTime) -> Vec<SlaTimer> {
        let mut guard = self.lock();
        let mut due = Vec::new();
        for entry in guard.values_mut() {
            if entry.fired || entry.timer.due > now {
                continue;
            }
            entry.fired = true;
            due.push(entry.timer.clone());
        }
        due
    }
}

/// One SLA mapping in the engine adapter: the synthetic node id and the task's real coordinates.
#[derive(Debug, Clone)]
struct Mapping {
    synthetic: NodeId,
    case: CaseId,
    real_case: String,
    real_node: String,
}

/// An adapter that runs the SLA on the engine's [`TimerPort`], so its timers live where the
/// engine's do (the host's route to `spot-scheduler`).
///
/// Each task gets a **synthetic** node id (`sla_<n>`) so its SLA timer never collides with the
/// Decision node's own human wait on the same `(case, node)`. The task id and the step travel in
/// the registration's `human` and `event` fields.
#[derive(Debug)]
pub struct EngineSlaTimers<T: TimerPort> {
    inner: Arc<T>,
    mappings: Arc<Mutex<BTreeMap<TaskId, Mapping>>>,
    counter: AtomicU64,
}

impl<T: TimerPort> EngineSlaTimers<T> {
    /// Wraps the engine's timer port.
    #[must_use]
    pub fn new(inner: Arc<T>) -> Self {
        Self {
            inner,
            mappings: Arc::new(Mutex::new(BTreeMap::new())),
            counter: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<TaskId, Mapping>> {
        self.mappings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<T: TimerPort + 'static> SlaTimerPort for EngineSlaTimers<T> {
    fn register(&self, timer: SlaTimer) {
        let Ok(case) = CaseId::new(&timer.case) else {
            return;
        };
        let synthetic = {
            let mut mappings = self.lock();
            if let Some(mapping) = mappings.get(&timer.task) {
                mapping.synthetic.clone()
            } else {
                let index = self.counter.fetch_add(1, Ordering::Relaxed);
                let node = NodeId::new(&format!("sla_{index}")).ok();
                let Some(node) = node else { return };
                mappings.insert(
                    timer.task.clone(),
                    Mapping {
                        synthetic: node.clone(),
                        case: case.clone(),
                        real_case: timer.case.clone(),
                        real_node: timer.node.clone(),
                    },
                );
                node
            }
        };
        self.inner.register(TimerRegistration {
            case,
            node: synthetic,
            attempt: u32::try_from(timer.step)
                .unwrap_or(u32::MAX)
                .saturating_add(1),
            due: Some(timer.due),
            event: Some(format!("sla:{}", timer.step)),
            human: Some(timer.task.to_string()),
        });
    }

    fn clear(&self, task: &TaskId) {
        let mapping = self.lock().remove(task);
        if let Some(mapping) = mapping {
            self.inner.clear(&mapping.case, &mapping.synthetic);
        }
    }

    fn due(&self, now: OffsetDateTime) -> Vec<SlaTimer> {
        let registrations = self.inner.due(now);
        let mappings = self.lock();
        let mut due = Vec::new();
        for registration in registrations {
            let Some(step) = registration
                .event
                .as_deref()
                .and_then(|event| event.strip_prefix("sla:"))
                .and_then(|step| step.parse::<usize>().ok())
            else {
                continue;
            };
            let Some(task_text) = registration.human.as_deref() else {
                continue;
            };
            let Some(task) = TaskId::new(task_text) else {
                continue;
            };
            let Some(mapping) = mappings.get(&task) else {
                continue;
            };
            due.push(SlaTimer {
                task,
                case: mapping.real_case.clone(),
                node: mapping.real_node.clone(),
                step,
                due: registration.due.unwrap_or(now),
            });
        }
        due
    }
}
