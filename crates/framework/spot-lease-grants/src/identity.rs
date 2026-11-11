//! Who is asking: the node the transport authenticated, and what it asks for.

use std::fmt;

use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;

/// The longest request id this crate accepts: 128 characters, the bound `spot-http` puts on the
/// envelope's request id.
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// The request id of the transport envelope: the idempotency key of a lease request.
///
/// It comes from the envelope the transport parsed (the same value `spot-http` carries as
/// [`RequestId`](https://docs.rs/spot-http)), **never** from a field of the body: a
/// body-supplied id would let one node present another node's key, and a `(node, series,
/// request_id)` triple is what decides whether a re-request gets the range already granted.
///
/// The type is bounded — 1 to [`MAX_REQUEST_ID_LEN`] visible ASCII characters (0x21..=0x7E),
/// the rule `spot-http` applies — because the grant service remembers a bounded number of ids
/// per node and must not hold whatever a peer sends.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Accepts `value` when it is 1 to [`MAX_REQUEST_ID_LEN`] visible ASCII characters, and
    /// rejects anything else without panicking.
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > MAX_REQUEST_ID_LEN {
            return None;
        }
        if value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            Some(Self(value.to_owned()))
        } else {
            None
        }
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The node the connection authenticated as: the mutual-TLS certificate the hub's node
/// directory resolved (ADR-062 section 3).
///
/// This is deliberately **not** a field a sender can put on the wire. The sync layer builds it
/// from the transport's answer and hands it to [`GrantService::request`](crate::GrantService).
/// A [`LeaseRequest`] may carry a claimed node, but the claim is only ever compared with this
/// value; it is never used to decide who gets the range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportNode {
    node: NodeId,
}

impl TransportNode {
    /// The identity of the node the transport authenticated.
    ///
    /// Call this with the node the certificate resolved to, never with a value read from a
    /// request payload.
    #[must_use]
    pub fn authenticated(node: NodeId) -> Self {
        Self { node }
    }

    /// The authenticated node.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }
}

/// A node's request for a range of one series.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRequest {
    /// The naming series.
    pub series: SeriesKey,
    /// How many numbers the node asks for.
    pub size: u64,
    /// The node the payload says it is. Optional; when present it must equal the
    /// authenticated node, or the request is refused and audited with both ids.
    pub claimed_node: Option<NodeId>,
    /// The transport envelope's [`RequestId`], when the envelope carried a usable one.
    ///
    /// The same `(node, series, request_id)` asks for the same range again: the service answers
    /// from the grant log instead of taking a new one, so a lost response costs nothing. A
    /// request without an id is granted a fresh range every time (the rate limit still applies);
    /// the sync layer always sets it from the envelope, and never from the body.
    pub request_id: Option<RequestId>,
}
