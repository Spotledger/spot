//! The event and the opaque principal used for authorisation.

/// An opaque application user: an id string plus a `guest` flag.
///
/// `spot-realtime` never interprets the id — permission rules (WP-PLT-03) and the transport
/// layer decide who a principal is; the bus only passes it to the
/// [`RoomAuthorizer`](crate::RoomAuthorizer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    id: String,
    guest: bool,
}

impl Principal {
    /// Wraps an arbitrary principal id with an explicit guest flag.
    pub fn new(id: impl Into<String>, guest: bool) -> Self {
        Self {
            id: id.into(),
            guest,
        }
    }

    /// A named (authenticated) user principal.
    pub fn user(id: impl Into<String>) -> Self {
        Self::new(id, false)
    }

    /// An unauthenticated guest principal.
    pub fn guest() -> Self {
        Self::new("guest", true)
    }

    /// The principal id, exactly as given.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether this principal is a guest.
    pub fn is_guest(&self) -> bool {
        self.guest
    }
}

/// A small realtime notification. It identifies what changed — never carries document data.
///
/// Per ADR-25 the payload stays small (identifiers and a hint); clients refetch the real
/// content from the API. Events are cheap to clone because the payload is shared.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// The room this event was published to.
    pub room: crate::Room,
    /// Strictly increasing per `(site, room)` and **never going backwards**, not even
    /// across a room free: the bus seeds every room incarnation's counter at its current
    /// high-water mark (see the crate docs), so a new incarnation hands out seqs strictly
    /// above all earlier ones, and the bus's very first room counts from 1. A resume
    /// point from an older incarnation therefore lands strictly below the new
    /// incarnation's `lost_before` watermark — or beyond its counter — and yields
    /// `ResyncRequired` rather than a silent skip.
    pub seq: u64,
    /// What happened, for example `doc_updated`. Free-form for now; the native API contract
    /// (WP-CON-03 and later) pins the vocabulary.
    pub kind: String,
    /// A small notification payload, shared (`Arc`) between subscribers.
    pub payload: std::sync::Arc<serde_json::Value>,
    /// Wall-clock publish time, from [`Clock::now_utc`](spot_core::Clock). For display and
    /// debugging; the bus ages the replay buffer with the clock's monotonic time.
    pub published_at: time::OffsetDateTime,
}
