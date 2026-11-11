//! The audit side of a transition: exactly one event per transition, and the sink that takes it.
//!
//! ADR-067 section 1 makes every transition an audit event, so the history of a branch's troubles
//! is part of the replayable record (ADR-061 section 2); the support agent and auditors read it
//! from the journal. This module is the event's runtime form and the trait a caller wires to the
//! journal's audit writer. It writes nothing itself.

use serde::{Deserialize, Serialize};

use crate::id::ConditionId;
use crate::millis::Millis;
use crate::params::ConditionParams;
use crate::severity::Severity;
use crate::subject::Subject;

/// The two audit topics ADR-061 section 2 reserves for conditions.
///
/// An escalation is a raise at a higher severity, so it goes on
/// [`ConditionTopic::Raised`]: the ADR has no third topic and an auditor reads "it got worse" as
/// the condition being raised again, not as a new condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionTopic {
    /// `sys_condition_raised`.
    Raised,
    /// `sys_condition_cleared`.
    Cleared,
}

impl ConditionTopic {
    /// The topic name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "sys_condition_raised",
            Self::Cleared => "sys_condition_cleared",
        }
    }
}

/// Which transition happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transition {
    /// The condition opened.
    Raised,
    /// The open condition's severity rose on its schedule.
    Escalated,
    /// The condition closed.
    Cleared,
}

impl Transition {
    /// The transition's name, as the audit event and a history entry spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::Escalated => "escalated",
            Self::Cleared => "cleared",
        }
    }
}

/// One transition, as the audit record carries it.
///
/// `severity` is the severity **after** the transition: an auditor reading the events in order
/// sees the condition's severity at every point in its life without replaying the schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transition", rename_all = "snake_case")]
pub enum AuditEvent {
    /// The condition opened on its subject.
    Raised {
        /// The condition.
        id: ConditionId,
        /// What it is about.
        subject: Subject,
        /// The severity it opened at.
        severity: Severity,
        /// When it opened.
        at_millis: Millis,
        /// The values its explanation carries.
        params: ConditionParams,
    },

    /// The open condition's severity rose.
    Escalated {
        /// The condition.
        id: ConditionId,
        /// What it is about.
        subject: Subject,
        /// The severity before the step.
        from: Severity,
        /// The severity after the step.
        to: Severity,
        /// When the step applied.
        at_millis: Millis,
        /// The values its explanation carries.
        params: ConditionParams,
    },

    /// The condition closed.
    Cleared {
        /// The condition.
        id: ConditionId,
        /// What it is about.
        subject: Subject,
        /// The severity it closed at.
        severity: Severity,
        /// When it was observed clear; for a debounced condition, the clear is recorded at the
        /// time it was observed, not when the window closed.
        at_millis: Millis,
        /// The values its explanation carried.
        params: ConditionParams,
    },
}

impl AuditEvent {
    /// Which transition this event records.
    #[must_use]
    pub const fn transition(&self) -> Transition {
        match self {
            Self::Raised { .. } => Transition::Raised,
            Self::Escalated { .. } => Transition::Escalated,
            Self::Cleared { .. } => Transition::Cleared,
        }
    }

    /// The audit topic this event is published on (ADR-061 section 2).
    #[must_use]
    pub const fn topic(&self) -> ConditionTopic {
        match self {
            Self::Raised { .. } | Self::Escalated { .. } => ConditionTopic::Raised,
            Self::Cleared { .. } => ConditionTopic::Cleared,
        }
    }

    /// The condition.
    #[must_use]
    pub const fn id(&self) -> &ConditionId {
        match self {
            Self::Raised { id, .. } | Self::Escalated { id, .. } | Self::Cleared { id, .. } => id,
        }
    }

    /// What the condition is about.
    #[must_use]
    pub const fn subject(&self) -> &Subject {
        match self {
            Self::Raised { subject, .. }
            | Self::Escalated { subject, .. }
            | Self::Cleared { subject, .. } => subject,
        }
    }

    /// The severity after the transition.
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::Raised { severity, .. } | Self::Cleared { severity, .. } => *severity,
            Self::Escalated { to, .. } => *to,
        }
    }

    /// When the transition happened.
    #[must_use]
    pub const fn at_millis(&self) -> Millis {
        match self {
            Self::Raised { at_millis, .. }
            | Self::Escalated { at_millis, .. }
            | Self::Cleared { at_millis, .. } => *at_millis,
        }
    }

    /// The values the explanation carries.
    #[must_use]
    pub const fn params(&self) -> &ConditionParams {
        match self {
            Self::Raised { params, .. }
            | Self::Escalated { params, .. }
            | Self::Cleared { params, .. } => params,
        }
    }
}

/// Where transitions go: the caller wires this to the journal's audit writer (ADR-061 section 2).
///
/// Recording is infallible on purpose. The transition has already happened in memory, so a sink
/// that could refuse would need a policy for a transition that happened but could not be
/// recorded; instead the writer behind the sink keeps its own durable queue, as the journal
/// writer does, and the engine stays a pure state machine.
pub trait AuditSink {
    /// Record one transition. Called exactly once per transition, in the order they happened.
    fn record(&mut self, event: AuditEvent);
}

/// A sink that keeps every event in memory, for tests and for a caller that drains it.
#[derive(Debug, Clone, Default)]
pub struct MemorySink {
    events: Vec<AuditEvent>,
}

impl MemorySink {
    /// A sink holding nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The events recorded so far, in transition order.
    #[must_use]
    pub fn events(&self) -> &[AuditEvent] {
        &self.events
    }

    /// How many events were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Take the events recorded so far, leaving the sink empty.
    pub fn drain(&mut self) -> Vec<AuditEvent> {
        std::mem::take(&mut self.events)
    }
}

impl AuditSink for MemorySink {
    fn record(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}
