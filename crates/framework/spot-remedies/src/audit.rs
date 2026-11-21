//! The audit trail: every remedy decision is recorded, allowed or refused, and undo is audited
//! like any other change.
//!
//! The framework never logs a secret or a parameter value: an [`AuditEvent`] carries the remedy
//! id, the actor, the instant and a plain-language detail, never the arguments. The host wires a
//! [`AuditSink`] to the audit chain; the tests wire [`MemoryAudit`].

use crate::model::RemedyId;

/// What an audit event records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditKind {
    /// The authoriser allowed the remedy.
    Authorised,
    /// The authoriser refused the remedy.
    Refused,
    /// A dry-run produced a plan.
    Planned,
    /// An apply ran.
    Applied,
    /// An undo ran.
    Undone,
    /// An undo was refused (unknown, expired, conflicting or irreversible).
    UndoRefused,
}

impl AuditKind {
    /// The stable snake_case text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuditKind::Authorised => "authorised",
            AuditKind::Refused => "refused",
            AuditKind::Planned => "planned",
            AuditKind::Applied => "applied",
            AuditKind::Undone => "undone",
            AuditKind::UndoRefused => "undo_refused",
        }
    }
}

/// One audit event: the remedy, who acted, when, and a detail that names no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// What happened.
    pub kind: AuditKind,
    /// The remedy the event is about.
    pub remedy: RemedyId,
    /// The actor (an agent id).
    pub actor: String,
    /// The instant (unix seconds).
    pub at: i64,
    /// A plain-language detail; never a parameter value or a secret.
    pub detail: String,
}

impl AuditEvent {
    /// An audit event.
    #[must_use]
    pub fn new(kind: AuditKind, remedy: &RemedyId, actor: &str, at: i64, detail: &str) -> Self {
        Self {
            kind,
            remedy: remedy.clone(),
            actor: actor.to_owned(),
            at,
            detail: detail.to_owned(),
        }
    }
}

/// Where audit events go. The host wires the audit chain; the tests collect them.
pub trait AuditSink {
    /// Records one event.
    fn record(&mut self, event: AuditEvent);
}

/// An in-memory audit sink for tests.
#[derive(Clone, Debug, Default)]
pub struct MemoryAudit {
    events: Vec<AuditEvent>,
}

impl MemoryAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event, in order.
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

    /// The kinds recorded, in order.
    #[must_use]
    pub fn kinds(&self) -> Vec<AuditKind> {
        self.events.iter().map(|event| event.kind).collect()
    }

    /// How many events of a kind were recorded.
    #[must_use]
    pub fn count(&self, kind: AuditKind) -> usize {
        self.events
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }
}

impl AuditSink for MemoryAudit {
    fn record(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}
