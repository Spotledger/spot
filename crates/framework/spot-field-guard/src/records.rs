//! The records the guard emits and the sinks it hands them to (ADR-070 section 12, rule 9).
//!
//! A record **never contains a value**: who, when, which document and field, through which
//! door, and whether it was authorised. A sink takes the record by value and answers at once
//! (accepted or full); the guard never waits on a slow consumer, and what a full queue means
//! is the guard's `fail_closed` flag, not the sink's.

use std::collections::VecDeque;
use std::sync::Mutex;

use spot_app_api::prelude::OffsetDateTime;

use crate::channel::Channel;

/// The door a value was asked for through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// One of the guarded channels.
    Channel(Channel),
    /// The audited reveal API.
    Api,
    /// A job, sync or migration acting as the platform (`FieldGuard::system_access`).
    System,
}

impl Via {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Channel(c) => c.as_str(),
            Via::Api => "api",
            Via::System => "system",
        }
    }
}

/// One access to a field of a `trace` protection group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldAccessRecord {
    /// The acting user's name (empty for the system).
    pub who: String,
    /// When.
    pub at: OffsetDateTime,
    /// The DocType the access was on.
    pub doctype: String,
    /// The document id text (`None` when the channel has no single document).
    pub id: Option<String>,
    /// The field path (`table.column` for a child-table column).
    pub field: String,
    /// The door.
    pub via: Via,
    /// Whether the access was allowed. A refusal is traced too.
    pub authorised: bool,
}

/// One reveal, or one refused attempt, or one undeclared system access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditRecord {
    /// A single-document reveal through the API.
    Reveal {
        /// The acting user's name.
        who: String,
        /// When.
        at: OffsetDateTime,
        /// The DocType.
        doctype: String,
        /// The document id as the caller wrote it.
        id: String,
        /// The field.
        field: String,
        /// The reason text the caller gave.
        reason: String,
        /// Whether the value was released.
        allowed: bool,
        /// Always the API.
        via: Via,
    },
    /// A bulk reveal by a system actor: the count, not the documents.
    RevealBulk {
        /// The acting actor's name (empty for the system itself).
        who: String,
        /// When.
        at: OffsetDateTime,
        /// The DocType.
        doctype: String,
        /// The field.
        field: String,
        /// The reason text.
        reason: String,
        /// How many documents were asked for.
        count: usize,
        /// Whether the values were released.
        allowed: bool,
    },
    /// A system actor touched a protection group its manifest does not declare.
    UndeclaredSystemAccess {
        /// The actor's manifest name.
        actor: String,
        /// When.
        at: OffsetDateTime,
        /// The DocType.
        doctype: String,
        /// The field.
        field: String,
        /// The undeclared group.
        group: String,
    },
}

/// What a sink did with a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// The record is queued.
    Accepted,
    /// The queue is full; the record was **not** kept.
    Full,
}

/// Where field access records go. Takes the record by value and answers without waiting.
pub trait TraceSink: Send + Sync {
    /// Queues one record.
    fn offer(&self, record: FieldAccessRecord) -> Offer;
}

/// Where audit records go (reveals, refusals, undeclared system access).
pub trait AuditSink: Send + Sync {
    /// Queues one record.
    fn offer(&self, record: AuditRecord) -> Offer;
}

/// A bounded in-memory queue that is both sinks: the reference implementation, and what the
/// tests read back. A server wires its own sink onto its journal; the contract is the same.
#[derive(Debug)]
pub struct BoundedQueue<T> {
    capacity: usize,
    items: Mutex<VecDeque<T>>,
}

impl<T> BoundedQueue<T> {
    /// A queue that holds at most `capacity` records (0 is always full).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        BoundedQueue {
            capacity,
            items: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, item: T) -> Offer {
        let mut guard = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() >= self.capacity {
            Offer::Full
        } else {
            guard.push_back(item);
            Offer::Accepted
        }
    }

    /// Takes every queued record, oldest first.
    pub fn drain(&self) -> Vec<T> {
        let mut guard = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.drain(..).collect()
    }

    /// How many records are queued.
    pub fn len(&self) -> usize {
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TraceSink for BoundedQueue<FieldAccessRecord> {
    fn offer(&self, record: FieldAccessRecord) -> Offer {
        self.push(record)
    }
}

impl AuditSink for BoundedQueue<AuditRecord> {
    fn offer(&self, record: AuditRecord) -> Offer {
        self.push(record)
    }
}

/// A sink that drops everything and says so: for tests of a guard that must refuse.
#[derive(Clone, Copy, Debug, Default)]
pub struct FullSink;

impl TraceSink for FullSink {
    fn offer(&self, _record: FieldAccessRecord) -> Offer {
        Offer::Full
    }
}

impl AuditSink for FullSink {
    fn offer(&self, _record: AuditRecord) -> Offer {
        Offer::Full
    }
}
