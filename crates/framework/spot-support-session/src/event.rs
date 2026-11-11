//! The audit event: one per transition, journalable, never carrying credentials or
//! action arguments.

use serde::{Deserialize, Serialize};

/// One transition of a session grant, for the audit trail.
///
/// Every transition returns one ([`Requested`](SessionEvent::Requested) is returned by
/// the constructor). Events carry actor, time and reason only: no credentials, no
/// tokens, and no arguments of any action that was checked (ADR-070 section 8, rule 4).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum SessionEvent {
    /// The grant was requested (the actor is the requester).
    Requested {
        /// Who asked for the session.
        actor: String,
        /// When (unix seconds).
        at: i64,
        /// The request's reason.
        reason: String,
    },
    /// The grant was approved and became active for its window.
    Approved {
        /// Who approved the session.
        actor: String,
        /// When (unix seconds).
        at: i64,
        /// The approval's reason.
        reason: String,
    },
    /// The grant was extended (a new approval) and its end moved later.
    Extended {
        /// Who approved the extension.
        actor: String,
        /// When (unix seconds).
        at: i64,
        /// The extension's reason.
        reason: String,
    },
    /// The grant was revoked, final and immediate.
    Revoked {
        /// Who revoked the session, tenant side or the subject.
        actor: String,
        /// When (unix seconds).
        at: i64,
        /// Why the session was revoked.
        reason: String,
    },
    /// The grant is expired; this derives from the times, and a caller journals it when
    /// it observes the expiry.
    Expired {
        /// Recorded by the observer (the crate derives expiry; there is no actor).
        actor: String,
        /// When (unix seconds).
        at: i64,
        /// The reason, empty or the caller's note (expiry has no reason of its own).
        reason: String,
    },
    /// A check refused an action; the caller journals the refusal.
    Refused {
        /// The stable `session.*` refusal code (the refusal's `code()`).
        code: String,
        /// When (unix seconds).
        at: i64,
        /// The refusal's plain sentence.
        reason: String,
    },
}

impl SessionEvent {
    /// An extension event from its history entry.
    #[must_use]
    pub fn extended(extension: &crate::model::Extension) -> SessionEvent {
        SessionEvent::Extended {
            actor: extension.approver.clone(),
            at: extension.at,
            reason: format!("extended to {}", crate::format_epoch(extension.extended_to)),
        }
    }

    /// The event kind, stable text for selectors and tests.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            SessionEvent::Requested { .. } => "requested",
            SessionEvent::Approved { .. } => "approved",
            SessionEvent::Extended { .. } => "extended",
            SessionEvent::Revoked { .. } => "revoked",
            SessionEvent::Expired { .. } => "expired",
            SessionEvent::Refused { .. } => "refused",
        }
    }

    /// The event's actor, empty for `Expired` when the observer left it empty.
    #[must_use]
    pub fn actor(&self) -> &str {
        match self {
            SessionEvent::Requested { actor, .. }
            | SessionEvent::Approved { actor, .. }
            | SessionEvent::Extended { actor, .. }
            | SessionEvent::Revoked { actor, .. }
            | SessionEvent::Expired { actor, .. } => actor,
            SessionEvent::Refused { .. } => "",
        }
    }

    /// The event's time (unix seconds).
    #[must_use]
    pub fn at(&self) -> i64 {
        match self {
            SessionEvent::Requested { at, .. }
            | SessionEvent::Approved { at, .. }
            | SessionEvent::Extended { at, .. }
            | SessionEvent::Revoked { at, .. }
            | SessionEvent::Expired { at, .. }
            | SessionEvent::Refused { at, .. } => *at,
        }
    }

    /// The event's reason text.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            SessionEvent::Requested { reason, .. }
            | SessionEvent::Approved { reason, .. }
            | SessionEvent::Extended { reason, .. }
            | SessionEvent::Revoked { reason, .. }
            | SessionEvent::Expired { reason, .. }
            | SessionEvent::Refused { reason, .. } => reason,
        }
    }

    /// A refusal, as the event the caller can journal.
    #[must_use]
    pub fn from_refusal(refusal: &crate::check::Refusal, now: i64) -> SessionEvent {
        SessionEvent::Refused {
            code: refusal.code().to_owned(),
            at: now,
            reason: refusal.to_string(),
        }
    }
}
