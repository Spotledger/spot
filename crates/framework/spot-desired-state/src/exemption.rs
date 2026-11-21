//! Acknowledged drift: an intentional difference with a reason, an owner and an expiry.
//!
//! Some differences are deliberate: a kernel version pinned for a certification, a service
//! disabled on an appliance. An [`ExemptionBook`] records those per server and subject, with
//! the reason and the person who accepted it. Every acknowledgement, revocation and expiry is
//! written to an [`AuditSink`], so the decision is visible in the audit trail rather than lost
//! in a comment.
//!
//! An exemption never becomes permanent by accident: `expires_at` is mandatory and the drift
//! alarms again at exactly that instant. Time is an argument; nothing here reads a clock.

use serde::{Deserialize, Serialize};
use spot_fleet_rollout::NodeId;

use crate::drift::DriftSubject;
use crate::error::Error;

/// What happened to an exemption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExemptionAction {
    /// A person acknowledged the drift.
    Acknowledged,
    /// A person withdrew the acknowledgement.
    Revoked,
    /// The acknowledgement passed its expiry.
    Expired,
}

/// One audit record for an exemption decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// What happened.
    pub action: ExemptionAction,
    /// The server.
    pub server: NodeId,
    /// What drifted.
    pub subject: DriftSubject,
    /// Why the drift was accepted, in the acknowledger's words.
    pub reason: String,
    /// Who acknowledged, revoked or observed the expiry.
    pub actor: String,
    /// When, in unix microseconds, supplied by the caller.
    pub at: i64,
}

/// Where exemption decisions are audited.
pub trait AuditSink {
    /// Records one decision.
    fn record(&mut self, event: AuditEvent);
}

/// An in-memory [`AuditSink`], for tests and for a control plane that forwards to the real
/// audit chain.
#[derive(Clone, Debug, Default)]
pub struct MemoryAudit {
    events: Vec<AuditEvent>,
}

impl MemoryAudit {
    /// An empty trail.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event, in the order it was recorded.
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
}

impl AuditSink for MemoryAudit {
    fn record(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}

/// One acknowledged drift on one server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exemption {
    /// The server.
    pub server: NodeId,
    /// What drifted.
    pub subject: DriftSubject,
    /// Why the difference is intentional.
    pub reason: String,
    /// Who accepted it.
    pub granted_by: String,
    /// When it was accepted (unix microseconds).
    pub granted_at: i64,
    /// When it stops applying (unix microseconds); the drift alarms at exactly this instant.
    pub expires_at: i64,
    /// When it was withdrawn, if it was.
    pub revoked_at: Option<i64>,
}

impl Exemption {
    /// Whether the exemption is active at `now`: not withdrawn and not yet expired.
    #[must_use]
    pub fn is_active_at(&self, now: i64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// A request to acknowledge one drift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExemptionRequest {
    /// The server.
    pub server: NodeId,
    /// What drifted.
    pub subject: DriftSubject,
    /// Why the difference is intentional.
    pub reason: String,
    /// Who accepts it.
    pub granted_by: String,
    /// When it stops applying (unix microseconds); must be later than `now`.
    pub expires_at: i64,
}

impl ExemptionRequest {
    /// A request for `server` and `subject`.
    #[must_use]
    pub fn new(
        server: NodeId,
        subject: DriftSubject,
        reason: impl Into<String>,
        granted_by: impl Into<String>,
        expires_at: i64,
    ) -> Self {
        Self {
            server,
            subject,
            reason: reason.into(),
            granted_by: granted_by.into(),
            expires_at,
        }
    }
}

/// The acknowledged drifts of a fleet.
#[derive(Clone, Debug, Default)]
pub struct ExemptionBook {
    entries: Vec<Exemption>,
}

impl ExemptionBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acknowledges the drift in `request`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidExemption`] when the reason or the acknowledger is empty, or when the
    /// expiry is not later than `now` (an acknowledgement that is already expired is a mistake,
    /// not an exemption).
    pub fn acknowledge(
        &mut self,
        request: ExemptionRequest,
        now: i64,
        audit: &mut dyn AuditSink,
    ) -> Result<Exemption, Error> {
        let ExemptionRequest {
            server,
            subject,
            reason,
            granted_by,
            expires_at,
        } = request;
        if reason.trim().is_empty() {
            return Err(Error::InvalidExemption {
                reason: "an exemption needs a reason".to_owned(),
            });
        }
        if granted_by.trim().is_empty() {
            return Err(Error::InvalidExemption {
                reason: "an exemption needs the person who granted it".to_owned(),
            });
        }
        if expires_at <= now {
            return Err(Error::InvalidExemption {
                reason: "the expiry must be later than the acknowledgement".to_owned(),
            });
        }
        let exemption = Exemption {
            server,
            subject,
            reason: reason.clone(),
            granted_by: granted_by.clone(),
            granted_at: now,
            expires_at,
            revoked_at: None,
        };
        self.entries.push(exemption.clone());
        audit.record(AuditEvent {
            action: ExemptionAction::Acknowledged,
            server: exemption.server.clone(),
            subject: exemption.subject.clone(),
            reason,
            actor: granted_by,
            at: now,
        });
        Ok(exemption)
    }

    /// Withdraws the latest active acknowledgement for `server` and `subject`.
    ///
    /// Returns whether one was found. The revocation is audited either way only when it
    /// happened.
    pub fn revoke(
        &mut self,
        server: &NodeId,
        subject: &DriftSubject,
        actor: impl Into<String>,
        now: i64,
        audit: &mut dyn AuditSink,
    ) -> bool {
        let actor = actor.into();
        for exemption in self.entries.iter_mut().rev() {
            if &exemption.server == server
                && &exemption.subject == subject
                && exemption.is_active_at(now)
            {
                exemption.revoked_at = Some(now);
                audit.record(AuditEvent {
                    action: ExemptionAction::Revoked,
                    server: exemption.server.clone(),
                    subject: exemption.subject.clone(),
                    reason: exemption.reason.clone(),
                    actor,
                    at: now,
                });
                return true;
            }
        }
        false
    }

    /// Whether an active acknowledgement covers `server` and `subject` at `now`.
    #[must_use]
    pub fn is_exempt(&self, server: &NodeId, subject: &DriftSubject, now: i64) -> bool {
        self.entries.iter().rev().any(|entry| {
            &entry.server == server && &entry.subject == subject && entry.is_active_at(now)
        })
    }

    /// Removes every expired or withdrawn entry and audits each expiry.
    pub fn purge_expired(&mut self, now: i64, audit: &mut dyn AuditSink) -> Vec<Exemption> {
        let mut kept = Vec::with_capacity(self.entries.len());
        let mut removed = Vec::new();
        for entry in self.entries.drain(..) {
            if entry.is_active_at(now) {
                kept.push(entry);
            } else if entry.revoked_at.is_none() {
                audit.record(AuditEvent {
                    action: ExemptionAction::Expired,
                    server: entry.server.clone(),
                    subject: entry.subject.clone(),
                    reason: entry.reason.clone(),
                    actor: "expiry".to_owned(),
                    at: now,
                });
                removed.push(entry);
            }
        }
        self.entries = kept;
        removed
    }

    /// Every entry active at `now`, in insertion order.
    #[must_use]
    pub fn active(&self, now: i64) -> Vec<&Exemption> {
        self.entries
            .iter()
            .filter(|entry| entry.is_active_at(now))
            .collect()
    }

    /// Every entry, including expired and withdrawn ones.
    #[must_use]
    pub fn entries(&self) -> &[Exemption] {
        &self.entries
    }

    /// How many entries the book holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the book is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
