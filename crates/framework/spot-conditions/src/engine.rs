//! The lifecycle of every open condition.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::audit::{AuditEvent, AuditSink, Transition};
use crate::error::ConditionsError;
use crate::health::{HealthSummary, HealthThresholds};
use crate::id::ConditionId;
use crate::millis::Millis;
use crate::params::ConditionParams;
use crate::registry::Registry;
use crate::severity::Severity;
use crate::subject::Subject;

/// The ADR-067 section 1 default debounce window: 60 seconds.
pub const DEFAULT_DEBOUNCE_MILLIS: Millis = Millis::new(60_000);

/// What [`ConditionEngine::raise`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseOutcome {
    /// A new condition opened: one `Raised` audit event was recorded.
    Raised,
    /// The condition was already open. Its parameters and `last_seen` were updated and **no**
    /// audit event was recorded: one open condition per `(id, subject)` (ADR-067 section 1).
    Updated,
    /// The condition had been observed clear but came back inside the debounce window, so it is
    /// the same condition: no audit event was recorded.
    Debounced,
}

/// What [`ConditionEngine::observe_cleared`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearOutcome {
    /// The open condition moved to pending. The `Cleared` event waits for the debounce window to
    /// close, and [`ConditionEngine::tick`] records it if the condition does not come back.
    Pending,
    /// Nothing was open for the key, so nothing changed.
    AlreadyClear,
}

/// What one [`ConditionEngine::tick`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Pending clears whose window had closed, each one `Cleared` event.
    pub cleared: usize,
    /// Escalation steps that came due, each one `Escalated` event.
    pub escalated: usize,
}

/// One open condition, as [`ConditionEngine::open`] and the health surface see it.
///
/// Serialises so a durable store (`spot-conditions-pg`) can save and reload it:
/// [`ConditionEngine::restore`] rebuilds the in-memory engine from exactly this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    /// The condition.
    pub id: ConditionId,
    /// What it is about.
    pub subject: Subject,
    /// Its severity now, after any escalation.
    pub severity: Severity,
    /// The values its explanation carries, as last raised.
    pub params: ConditionParams,
    /// When it opened; an escalation is measured from here, so a re-raise that de-duplicates
    /// does not restart the schedule.
    pub raised_at_millis: Millis,
    /// When it was last raised or confirmed open.
    pub last_seen_millis: Millis,
}

/// One recorded transition of a subject, in the order the engine made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The condition.
    pub id: ConditionId,
    /// What happened.
    pub transition: Transition,
    /// The severity after the transition.
    pub severity: Severity,
    /// When it happened.
    pub at_millis: Millis,
}

/// The identity of one condition: a definition id and the subject it is open on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    id: ConditionId,
    subject: Subject,
}

/// An open condition's state.
#[derive(Debug, Clone)]
struct OpenState {
    severity: Severity,
    params: ConditionParams,
    raised_at: Millis,
    last_seen: Millis,
}

/// A condition observed clear whose debounce window has not closed yet.
///
/// It keeps everything the `Cleared` event will need, so the event can carry the severity and
/// parameters the condition had when it was observed clear, at the time it was observed.
#[derive(Debug, Clone)]
struct PendingClear {
    severity: Severity,
    params: ConditionParams,
    raised_at: Millis,
    last_seen: Millis,
    cleared_at: Millis,
}

/// Either state of a key. A key in this map is either open or waiting to be recorded clear; a key
/// that is neither is removed, so the map never grows with conditions that are gone.
#[derive(Debug, Clone)]
enum ConditionState {
    Open(OpenState),
    Pending(PendingClear),
}

/// The conditions of one deployment: which are open, on what, since when, and how bad.
///
/// One engine owns one registry of definitions and one sink of audit events. Every operation
/// takes the current instant as an argument; the engine never reads a clock.
///
/// # Lifecycle
///
/// * [`raise`](Self::raise) opens a condition, or updates the open one in place.
/// * [`observe_cleared`](Self::observe_cleared) records that the detector no longer sees the
///   abnormal state. The clear is **pending** until the debounce window closes, so a condition
///   that flaps produces one condition and not a stream of them.
/// * [`tick`](Self::tick) closes the window on pending clears and applies escalation.
/// * [`open`](Self::open) is what is open now; [`history`](Self::history) is what happened to a
///   subject.
///
/// # De-duplication
///
/// At most one condition is open per `(id, subject)`. Raising an open one again replaces its
/// parameters and moves `last_seen` forward, and records nothing: the condition never went away,
/// so there is nothing new to audit.
///
/// # Debounce
///
/// A condition observed clear and raised again **strictly before** the window closes is the same
/// condition: it keeps its original `raised_at` (so its escalation does not restart), its
/// severity (so an escalation is never undone) and produces no events. At exactly the window's
/// end the clear is committed and the raise opens a new condition, at the definition's severity.
///
/// # Escalation
///
/// Each step of the definition's schedule raises the severity once the condition has been open
/// for the step's elapsed time, and records one `Escalated` event. Escalation never lowers: a
/// step whose target is not above the current severity is skipped, so a schedule cannot undo an
/// earlier one and a tick that repeats changes nothing.
#[derive(Debug, Clone)]
pub struct ConditionEngine<S: AuditSink> {
    registry: Registry,
    sink: S,
    debounce: Millis,
    states: BTreeMap<Key, ConditionState>,
    history: BTreeMap<Subject, Vec<HistoryEntry>>,
}

impl<S: AuditSink> ConditionEngine<S> {
    /// An engine over `registry` that reports to `sink`, with the default debounce window of
    /// 60 seconds (ADR-067 section 1: "a short interval").
    #[must_use]
    pub fn new(registry: Registry, sink: S) -> Self {
        Self::with_debounce(registry, sink, DEFAULT_DEBOUNCE_MILLIS)
    }

    /// An engine with a debounce window of `debounce`.
    ///
    /// The window is a tunable setting, not a hidden constant. [`Millis::ZERO`] disables
    /// debouncing: a clear is then committed by the next tick.
    #[must_use]
    pub fn with_debounce(registry: Registry, sink: S, debounce: Millis) -> Self {
        Self {
            registry,
            sink,
            debounce,
            states: BTreeMap::new(),
            history: BTreeMap::new(),
        }
    }

    /// Rebuilds an engine from a stored open set (`spot-conditions-pg`'s `load_open`), so a
    /// restart does not forget a condition that was open before it.
    ///
    /// Every entry becomes an [`ConditionState::Open`] exactly as the engine would have it in
    /// memory: its severity, parameters, `raised_at` and `last_seen` are taken as stored, not
    /// recomputed. Nothing is recorded to `sink`: a restore is not a transition, so it must
    /// never produce an audit event (a store replaying its own rows back through a new process
    /// would otherwise double the audit trail).
    ///
    /// # Errors
    ///
    /// [`ConditionsError::UnknownId`] when an entry's id is not registered in `registry`: a
    /// stored condition the current build no longer knows how to raise or escalate must not be
    /// adopted silently. [`ConditionsError::DuplicateId`] when two entries share the same
    /// `(id, subject)`: at most one condition is open per key (the same invariant
    /// [`ConditionEngine::raise`] keeps), so a store that returned two rows for one key is
    /// corrupt and must not be adopted as if it were ordinary de-duplication.
    pub fn restore(
        registry: Registry,
        sink: S,
        debounce: Millis,
        open: Vec<Open>,
    ) -> Result<Self, ConditionsError> {
        let mut engine = Self::with_debounce(registry, sink, debounce);
        for entry in open {
            if engine.registry.get(entry.id.as_str()).is_none() {
                return Err(ConditionsError::UnknownId {
                    id: entry.id.as_str().to_owned(),
                });
            }
            let key = Key {
                id: entry.id.clone(),
                subject: entry.subject.clone(),
            };
            if engine.states.contains_key(&key) {
                return Err(ConditionsError::DuplicateId {
                    id: entry.id.as_str().to_owned(),
                });
            }
            engine.states.insert(
                key,
                ConditionState::Open(OpenState {
                    severity: entry.severity,
                    params: entry.params,
                    raised_at: entry.raised_at_millis,
                    last_seen: entry.last_seen_millis,
                }),
            );
        }
        Ok(engine)
    }

    /// The definitions this engine knows.
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The sink the engine reports to.
    #[must_use]
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The debounce window.
    #[must_use]
    pub fn debounce(&self) -> Millis {
        self.debounce
    }

    /// Raise `id` on `subject`, with `params`, at `now`.
    ///
    /// # Errors
    ///
    /// [`ConditionsError::UnknownId`] when `id` is not a registered condition: an unregistered
    /// id is a programming mistake, and recording it would put a name into the audit trail that
    /// nothing can explain.
    pub fn raise(
        &mut self,
        id: &str,
        subject: &str,
        params: ConditionParams,
        now: Millis,
    ) -> Result<RaiseOutcome, ConditionsError> {
        let (key, base_severity) = self.resolve(id, subject)?;
        let existing = self.states.remove(&key);
        match existing {
            None => {
                self.states.insert(
                    key.clone(),
                    ConditionState::Open(OpenState {
                        severity: base_severity,
                        params: params.clone(),
                        raised_at: now,
                        last_seen: now,
                    }),
                );
                self.record(AuditEvent::Raised {
                    id: key.id,
                    subject: key.subject,
                    severity: base_severity,
                    at_millis: now,
                    params,
                });
                Ok(RaiseOutcome::Raised)
            }
            Some(ConditionState::Open(open)) => {
                self.states.insert(
                    key,
                    ConditionState::Open(OpenState {
                        params,
                        last_seen: open.last_seen.max(now),
                        ..open
                    }),
                );
                Ok(RaiseOutcome::Updated)
            }
            Some(ConditionState::Pending(pending)) => {
                if now.since(pending.cleared_at) < self.debounce {
                    // It came back inside the window: the same condition, never gone.
                    self.states.insert(
                        key,
                        ConditionState::Open(OpenState {
                            severity: pending.severity,
                            params,
                            raised_at: pending.raised_at,
                            last_seen: pending.last_seen.max(now),
                        }),
                    );
                    Ok(RaiseOutcome::Debounced)
                } else {
                    // The window closed first: commit the clear where it was observed, then
                    // raise a new condition at the definition's severity.
                    self.record(AuditEvent::Cleared {
                        id: key.id.clone(),
                        subject: key.subject.clone(),
                        severity: pending.severity,
                        at_millis: pending.cleared_at,
                        params: pending.params,
                    });
                    self.states.insert(
                        key.clone(),
                        ConditionState::Open(OpenState {
                            severity: base_severity,
                            params: params.clone(),
                            raised_at: now,
                            last_seen: now,
                        }),
                    );
                    self.record(AuditEvent::Raised {
                        id: key.id,
                        subject: key.subject,
                        severity: base_severity,
                        at_millis: now,
                        params,
                    });
                    Ok(RaiseOutcome::Raised)
                }
            }
        }
    }

    /// Record that `id` on `subject` is no longer in the abnormal state at `now`.
    ///
    /// The clear is pending for the debounce window: a condition that comes back inside it is
    /// the same condition and records nothing. [`tick`](Self::tick) commits the clear once the
    /// window closes, at the instant the clear was observed.
    ///
    /// # Errors
    ///
    /// [`ConditionsError::UnknownId`] when `id` is not a registered condition.
    pub fn observe_cleared(
        &mut self,
        id: &str,
        subject: &str,
        now: Millis,
    ) -> Result<ClearOutcome, ConditionsError> {
        let (key, _) = self.resolve(id, subject)?;
        match self.states.remove(&key) {
            Some(ConditionState::Open(open)) => {
                self.states.insert(
                    key,
                    ConditionState::Pending(PendingClear {
                        severity: open.severity,
                        params: open.params,
                        raised_at: open.raised_at,
                        last_seen: open.last_seen,
                        cleared_at: now,
                    }),
                );
                Ok(ClearOutcome::Pending)
            }
            Some(state @ ConditionState::Pending(_)) => {
                self.states.insert(key, state);
                Ok(ClearOutcome::AlreadyClear)
            }
            None => Ok(ClearOutcome::AlreadyClear),
        }
    }

    /// Advance the engine to `now`: commit the pending clears whose window has closed, at the
    /// time each was observed clear, then apply every escalation step that has come due.
    ///
    /// Idempotent: a tick with nothing due records nothing and changes nothing.
    pub fn tick(&mut self, now: Millis) -> TickReport {
        let mut report = TickReport::default();
        let due: Vec<Key> = self
            .states
            .iter()
            .filter(|(_, state)| match state {
                ConditionState::Pending(pending) => now.since(pending.cleared_at) >= self.debounce,
                ConditionState::Open(_) => false,
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            if let Some(ConditionState::Pending(pending)) = self.states.remove(&key) {
                self.record(AuditEvent::Cleared {
                    id: key.id,
                    subject: key.subject,
                    severity: pending.severity,
                    at_millis: pending.cleared_at,
                    params: pending.params,
                });
                report.cleared += 1;
            }
        }

        // Escalate what has been open long enough. The registry put the schedule in elapsed-time
        // order, so the first step that is not due ends the walk; a step whose target is not
        // above the current severity is skipped, which is what makes this idempotent and what
        // keeps an escalation from ever lowering a condition.
        let open_keys: Vec<Key> = self
            .states
            .iter()
            .filter(|(_, state)| matches!(state, ConditionState::Open(_)))
            .map(|(key, _)| key.clone())
            .collect();
        for key in open_keys {
            let schedule = self
                .registry
                .get(key.id.as_str())
                .map(|definition| definition.escalation.clone())
                .unwrap_or_default();
            if schedule.is_empty() {
                continue;
            }
            let Some(ConditionState::Open(mut open)) = self.states.remove(&key) else {
                continue;
            };
            for step in schedule {
                if !now.elapsed_at_least(open.raised_at, step.after_millis) {
                    break;
                }
                if step.to <= open.severity {
                    continue;
                }
                let from = open.severity;
                open.severity = step.to;
                let params = open.params.clone();
                self.record(AuditEvent::Escalated {
                    id: key.id.clone(),
                    subject: key.subject.clone(),
                    from,
                    to: step.to,
                    at_millis: now,
                    params,
                });
                report.escalated += 1;
            }
            self.states.insert(key, ConditionState::Open(open));
        }
        report
    }

    /// The conditions that are open now, in `(id, subject)` order.
    ///
    /// A condition observed clear but still inside its debounce window is **not** open: the
    /// abnormal state is gone. It is not recorded clear yet either, which is what
    /// [`tick`](Self::tick) settles.
    #[must_use]
    pub fn open(&self) -> Vec<Open> {
        self.states
            .iter()
            .filter_map(|(key, state)| match state {
                ConditionState::Open(open) => Some(Open {
                    id: key.id.clone(),
                    subject: key.subject.clone(),
                    severity: open.severity,
                    params: open.params.clone(),
                    raised_at_millis: open.raised_at,
                    last_seen_millis: open.last_seen,
                }),
                ConditionState::Pending(_) => None,
            })
            .collect()
    }

    /// The transitions recorded for `subject`, oldest first, across every condition.
    ///
    /// Empty when the subject never had a condition. The audit events are the durable record
    /// (ADR-061); this is the in-memory view for a surface that is showing one subject now.
    #[must_use]
    pub fn history(&self, subject: &str) -> &[HistoryEntry] {
        self.history
            .get(subject)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The calm-by-default summary of what is open at `now`: counts per severity, and the short
    /// list that needs a person (ADR-067 section 6).
    ///
    /// `thresholds` decides when an amber condition joins the list; a red one is on it at once.
    #[must_use]
    pub fn health(&self, now: Millis, thresholds: &HealthThresholds) -> HealthSummary {
        HealthSummary::of(&self.open(), now, thresholds)
    }

    /// The definition behind `id` and its raise severity, or the spelling of the id as text.
    fn resolve(&self, id: &str, subject: &str) -> Result<(Key, Severity), ConditionsError> {
        let condition = ConditionId::parse(id)?;
        let definition = self
            .registry
            .get(id)
            .ok_or_else(|| ConditionsError::UnknownId { id: id.to_owned() })?;
        let severity = definition.severity;
        Ok((
            Key {
                id: condition,
                subject: Subject::new(subject),
            },
            severity,
        ))
    }

    /// Append the event to the subject's history and hand it to the sink: the one place a
    /// transition is recorded, so "every transition is exactly one audit event" holds by
    /// construction.
    fn record(&mut self, event: AuditEvent) {
        self.history
            .entry(event.subject().clone())
            .or_default()
            .push(HistoryEntry {
                id: event.id().clone(),
                transition: event.transition(),
                severity: event.severity(),
                at_millis: event.at_millis(),
            });
        self.sink.record(event);
    }
}
