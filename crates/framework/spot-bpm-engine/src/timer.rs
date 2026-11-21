//! The [`TimerPort`]: registering a wait as a scheduler slot, and firing it exactly once.
//!
//! When a node returns [`NodeOutcome::Wait`](crate::NodeOutcome::Wait), the engine registers the
//! wait here — the host's route to `spot-scheduler` (WP-PLT-07) — and keeps the durable
//! [`TimerRecord`](crate::TimerRecord) in the Case store as the source of truth. The port
//! answers "which waits are due at this instant" exactly once per `(case, node)`, so a scheduler
//! tick that fires a slot twice is harmless and a missed tick is caught by the next `step`.
//!
//! [`InMemoryTimerPort`] is the reference implementation. [`NoTimers`] is a port that records
//! nothing, for an engine driven only by explicit signals.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::OffsetDateTime;
use spot_dag::NodeId;

use crate::handler::CaseId;

/// One wait registered with the timer port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerRegistration {
    /// The Case the wait belongs to.
    pub case: CaseId,
    /// The node waiting.
    pub node: NodeId,
    /// The attempt that registered the wait.
    pub attempt: u32,
    /// The instant the wait ends, when it has one.
    pub due: Option<OffsetDateTime>,
    /// The event key, for an event wait.
    pub event: Option<String>,
    /// The human-task key, for a human wait.
    pub human: Option<String>,
}

impl TimerRegistration {
    /// Whether the wait is due at `now`.
    #[must_use]
    pub fn is_due(&self, now: OffsetDateTime) -> bool {
        self.due.is_some_and(|due| due <= now)
    }
}

/// The engine's route to a scheduler: register, clear and fire waits.
pub trait TimerPort: Send + Sync {
    /// Registers a wait, replacing one already registered for the same `(case, node)`.
    fn register(&self, registration: TimerRegistration);

    /// Clears a node's wait, once it has fired or the Case ended.
    fn clear(&self, case: &CaseId, node: &NodeId);

    /// Every wait due at `now` and not yet fired, marking each fired exactly once.
    fn due(&self, now: OffsetDateTime) -> Vec<TimerRegistration>;

    /// Clears every wait of a Case (its cancellation).
    fn cancel_case(&self, case: &CaseId);

    /// How many times a `(case, node)` wait has fired; a test hook for exactly-once.
    fn fired_count(&self, case: &CaseId, node: &NodeId) -> u32;
}

/// One entry of the in-memory port: the registration and whether it has fired.
#[derive(Debug, Clone)]
struct Entry {
    registration: TimerRegistration,
    fired: u32,
}

/// The in-memory reference [`TimerPort`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryTimerPort {
    entries: Arc<Mutex<BTreeMap<(CaseId, NodeId), Entry>>>,
}

impl InMemoryTimerPort {
    /// An empty port.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(CaseId, NodeId), Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TimerPort for InMemoryTimerPort {
    fn register(&self, registration: TimerRegistration) {
        let mut guard = self.lock();
        let key = (registration.case.clone(), registration.node.clone());
        let replace = guard
            .get(&key)
            .is_none_or(|entry| registration.attempt >= entry.registration.attempt);
        if replace {
            guard.insert(
                key,
                Entry {
                    registration,
                    fired: 0,
                },
            );
        }
    }

    fn clear(&self, case: &CaseId, node: &NodeId) {
        self.lock().remove(&(case.clone(), node.clone()));
    }

    fn due(&self, now: OffsetDateTime) -> Vec<TimerRegistration> {
        let mut guard = self.lock();
        let mut due = Vec::new();
        for entry in guard.values_mut() {
            if !entry.registration.is_due(now) {
                continue;
            }
            // Fired exactly once: a second call for the same instant does not return it again
            // unless it was re-registered by a later attempt.
            if entry.fired > 0 {
                continue;
            }
            entry.fired += 1;
            due.push(entry.registration.clone());
        }
        due
    }

    fn cancel_case(&self, case: &CaseId) {
        self.lock().retain(|(candidate, _), _| candidate != case);
    }

    fn fired_count(&self, case: &CaseId, node: &NodeId) -> u32 {
        self.lock()
            .get(&(case.clone(), node.clone()))
            .map_or(0, |entry| entry.fired)
    }
}

/// A port that records nothing: an engine driven only by explicit signals and due instants
/// checked against its own durable timer records.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTimers;

impl TimerPort for NoTimers {
    fn register(&self, _registration: TimerRegistration) {}

    fn clear(&self, _case: &CaseId, _node: &NodeId) {}

    fn due(&self, _now: OffsetDateTime) -> Vec<TimerRegistration> {
        Vec::new()
    }

    fn cancel_case(&self, _case: &CaseId) {}

    fn fired_count(&self, _case: &CaseId, _node: &NodeId) -> u32 {
        0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn registration(due: OffsetDateTime) -> TimerRegistration {
        TimerRegistration {
            case: CaseId::new("case1").unwrap(),
            node: NodeId::new("n1").unwrap(),
            attempt: 1,
            due: Some(due),
            event: None,
            human: None,
        }
    }

    #[test]
    fn a_due_wait_fires_exactly_once() {
        let port = InMemoryTimerPort::new();
        port.register(registration(now()));
        assert_eq!(port.due(now()).len(), 1);
        assert_eq!(
            port.due(now()).len(),
            0,
            "the second call does not fire again"
        );
        assert_eq!(
            port.fired_count(&registration(now()).case, &registration(now()).node),
            1
        );
    }

    #[test]
    fn a_future_wait_is_not_due() {
        let port = InMemoryTimerPort::new();
        port.register(registration(now() + std::time::Duration::from_secs(60)));
        assert!(port.due(now()).is_empty());
    }

    #[test]
    fn clearing_a_wait_removes_it() {
        let port = InMemoryTimerPort::new();
        let reg = registration(now());
        port.register(reg.clone());
        port.clear(&reg.case, &reg.node);
        assert!(port.due(now()).is_empty());
    }

    #[test]
    fn no_timers_records_nothing() {
        let port = NoTimers;
        port.register(registration(now()));
        assert!(port.due(now()).is_empty());
    }
}
