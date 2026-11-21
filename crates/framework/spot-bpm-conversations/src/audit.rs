//! The audit trail of the receiving gate.
//!
//! A refusal is a decision about something a partner sent, and a decision that leaves no trace
//! cannot be reviewed afterwards. Every refusal writes one row through [`GateAudit`]: the stable
//! reason code and the ids it belongs to (link, conversation, message) — **never** a payload
//! value, a field name a peer chose, or any other text from the message.
//!
//! [`LinkAudit`] is the adapter over the link's tamper-evident chain (`spot-tenant-link`'s
//! [`AuditSink`]): it appends a `tenant_link.refused` record whose detail is the reason code.
//! [`MemoryGateAudit`] is the reference implementation for tests and tooling.

use std::sync::{Arc, Mutex, MutexGuard};

use spot_link_exchange::{Conversation, MessageId};
use spot_tenant_link::{AuditSink, LinkEvent, LinkEventKind, LinkId, record_event};

use crate::error::{ConversationError, Result};

/// One audited refusal: the reason and the ids, never a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The link the refused message came in on.
    pub link: LinkId,
    /// The conversation it belongs to.
    pub conversation: Conversation,
    /// The refused message's id.
    pub message_id: MessageId,
    /// The stable code of the reason.
    pub code: &'static str,
    /// Unix milliseconds the refusal was decided.
    pub at_ms: u64,
}

/// Where the receiving gate writes its refusals.
pub trait GateAudit: Send + Sync {
    /// Appends one row for a refused message.
    ///
    /// # Errors
    /// [`ConversationError::Audit`] when the trail could not be written; the gate then fails
    /// closed and takes nothing in.
    fn refused(&self, refusal: &Refusal) -> Result<()>;
}

/// The link's own audit chain, as the gate's trail.
///
/// The chain's vocabulary is the link's (`tenant_link.refused` with the reason code as its
/// detail), so a reviewer sees the id of the link and why a message was refused there, in the
/// same tamper-evident trail as every other use of the link.
pub struct LinkAudit {
    sink: Arc<dyn AuditSink>,
}

impl LinkAudit {
    /// A trail over `sink`.
    #[must_use]
    pub fn new(sink: Arc<dyn AuditSink>) -> Self {
        Self { sink }
    }
}

impl GateAudit for LinkAudit {
    fn refused(&self, refusal: &Refusal) -> Result<()> {
        record_event(
            &*self.sink,
            &LinkEvent {
                link: refusal.link,
                kind: LinkEventKind::Refused,
                actor: "system".to_owned(),
                at: refusal.at_ms,
                revision: 0,
                detail: Some(refusal.code),
            },
            "record a gate refusal",
        )
        .map_err(|_| ConversationError::Audit)
    }
}

/// Refusals kept in memory, in the order they were decided.
#[derive(Default)]
pub struct MemoryGateAudit {
    rows: Mutex<Vec<Refusal>>,
}

impl MemoryGateAudit {
    /// An empty trail.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every row, oldest first.
    #[must_use]
    pub fn rows(&self) -> Vec<Refusal> {
        self.lock().clone()
    }

    /// The reason codes of every row, oldest first.
    #[must_use]
    pub fn codes(&self) -> Vec<&'static str> {
        self.lock().iter().map(|row| row.code).collect()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Refusal>> {
        self.rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl GateAudit for MemoryGateAudit {
    fn refused(&self, refusal: &Refusal) -> Result<()> {
        self.lock().push(refusal.clone());
        Ok(())
    }
}
