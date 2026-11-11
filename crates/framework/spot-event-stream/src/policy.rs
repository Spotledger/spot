//! The engine's value types: the batch and park policies, the batch, the lease, the parked
//! report and the stream-rights seam. They live apart from [`crate::engine`] to keep it cohesive.

use std::time::Duration;

use spot_coordinator::{FencingToken, Lease};
use spot_events_contract::envelope::{EventEnvelope, Position};
use spot_storage_contract::FeedPosition;
use time::OffsetDateTime;

use crate::error::CODE_WINDOW_FULL;

/// Limits of one pull.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchPolicy {
    /// The most events one batch carries (a caller's smaller `limit` wins).
    pub max_events: usize,
    /// The most bytes one batch's serialised events may total.
    pub max_bytes: usize,
    /// The in-flight window: events delivered but not yet acked, per consumer.
    pub window: usize,
    /// The page size handed to the feed read.
    pub page_limit: u32,
    /// The host's long-poll budget for a `wait` pull. The engine spawns no timer and never
    /// blocks, so this is advice the host applies; it is here so one policy states every bound.
    pub max_wait: Duration,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self {
            max_events: 500,
            max_bytes: 4 * 1024 * 1024,
            window: 1_000,
            page_limit: 500,
            max_wait: Duration::from_secs(25),
        }
    }
}

/// When a stalled consumer is parked, and when a parked one is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParkPolicy {
    /// With unacknowledged events and no acknowledgement for this long, the partition is parked.
    pub park_after: Duration,
    /// A partition parked at least this long is named in [`crate::engine::StreamEngine::parked_report`].
    pub report_after: Duration,
}

impl Default for ParkPolicy {
    fn default() -> Self {
        Self {
            park_after: Duration::from_secs(300),
            report_after: Duration::from_secs(900),
        }
    }
}

/// One pull's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    /// The envelopes, in `(generation, seq, ordinal)` order within the partition.
    pub events: Vec<EventEnvelope>,
    /// The position to acknowledge for this batch; `None` when nothing was delivered. It is
    /// **not** committed: only [`crate::engine::StreamEngine::ack`] moves the checkpoint.
    pub next_position: Option<Position>,
    /// Events skipped by the permission filter (and by a filter that could not be evaluated).
    /// No subject is named.
    pub skipped: u64,
    /// The pull returned nothing because the in-flight window is full.
    pub window_full: bool,
    /// The pull returned nothing because the partition is parked.
    pub parked: bool,
}

impl Batch {
    /// An empty batch.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            events: Vec::new(),
            next_position: None,
            skipped: 0,
            window_full: false,
            parked: false,
        }
    }

    /// An empty batch flagged as window-full.
    #[must_use]
    pub fn window_full() -> Self {
        Self {
            window_full: true,
            ..Self::empty()
        }
    }

    /// An empty batch flagged as parked.
    #[must_use]
    pub fn parked() -> Self {
        Self {
            parked: true,
            ..Self::empty()
        }
    }

    /// Whether nothing was delivered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The `eventstream.*` code a host maps a non-delivering batch to, if any.
    #[must_use]
    pub fn error_code(&self) -> Option<&'static str> {
        if self.window_full {
            Some(CODE_WINDOW_FULL)
        } else {
            None
        }
    }
}

/// A held connection lease for one consumer. Holding it is what lets the holder pull and ack.
#[derive(Debug)]
pub struct ConsumerLease {
    /// The consumer name.
    pub consumer: String,
    /// The coordinator lease.
    pub lease: Lease,
}

impl ConsumerLease {
    /// The fencing token this connection writes under.
    #[must_use]
    pub fn token(&self) -> FencingToken {
        self.lease.token
    }
}

/// One parked partition, as reported to an operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedConsumer {
    /// The consumer name.
    pub consumer: String,
    /// The partition.
    pub partition: u32,
    /// When it was parked.
    pub since: OffsetDateTime,
}

/// The outbox head, when the backend can answer it. `None` ("unknown") leaves a `tail` start
/// at the feed's start rather than guessing.
pub trait FeedHead: Send + Sync {
    /// The current head of the outbox feed.
    fn head(&self) -> Option<FeedPosition>;
}

/// Whether an owner holds the explicit stream right for a reserved topic class.
pub trait StreamRights: Send + Sync {
    /// Whether `owner` may stream the class named by a reserved prefix (`doc_`, `sys_`,
    /// `identity_`).
    fn may_stream_class(&self, owner: &str, class: &str) -> bool;
}

/// A rights source that grants nothing: the safe default for a host that has not wired one.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyStreamRights;

impl StreamRights for DenyStreamRights {
    fn may_stream_class(&self, _owner: &str, _class: &str) -> bool {
        false
    }
}

/// A rights source that grants everything: the platform and the tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllStreamRights;

impl StreamRights for AllowAllStreamRights {
    fn may_stream_class(&self, _owner: &str, _class: &str) -> bool {
        true
    }
}
