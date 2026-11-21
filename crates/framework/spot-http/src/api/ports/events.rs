//! The event stream seam of `/api/v1/events/*` (`event_consumer_list`, `_put`, `_get`, `_delete`,
//! `_pull`, `_ack`, `_restart` and `event_stream`, contract 0.4.0, WP-INT-02.T4): the named
//! consumers of ADR-078 (delivery level **D1**, at-least-once and resumable).
//!
//! `spot-http` does not depend on the event-stream engine. It defines [`EventStreamPort`], the
//! operations in the shapes the router needs, and the host writes the adapter over
//! `spot-event-stream` (the engine, the durable consumer store, the fenced lease). Everything
//! that decides is the port's: who the caller is (from `ctx.credential` on `ctx.site`), which
//! consumers the caller may see, whether a position is the caller's own, which events the
//! consumer's owner may read. The routes pass **no user and no tenant**: there is no way to ask
//! for another caller's consumer through this trait.
//!
//! Positions cross the port as the contract's opaque `EventPosition` text (`v1.<token>`); the
//! routes check the shape (`^v1\.[A-Za-z0-9_-]{1,255}$`) and never read the inside. The lease
//! crosses as the opaque `X-Consumer-Lease` text. Events cross as the contract's
//! `EventEnvelope` JSON, already filtered and projected for the consumer's owner.
//!
//! Delivery is **at-least-once**: nothing here claims exactly-once, and a receiver deduplicates
//! on `event_id`.

use serde_json::Value as Json;

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`EventStreamPort`].
pub const KEY: PortKey = PortKey::of::<dyn EventStreamPort>();

/// What a port call answers.
pub type EventsResult<T> = Result<T, ApiError>;

/// How much of each event a consumer receives (the contract's `payload_mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadModeIn {
    /// Envelope, subject and meta; no field data.
    IdsOnly,
    /// The document fields the owner may read, masked or removed as everywhere else.
    Fields,
}

impl PayloadModeIn {
    /// The contract's text.
    pub fn as_str(self) -> &'static str {
        match self {
            PayloadModeIn::IdsOnly => "ids_only",
            PayloadModeIn::Fields => "fields",
        }
    }
}

/// Where a consumer or a restarted partition begins (the contract's `ConsumerStart`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartIn {
    /// Events from now on.
    Tail,
    /// The oldest the feed still holds.
    FromBeginning,
    /// A position previously returned for this consumer (`v1.<token>`).
    FromPosition(String),
}

/// The body of `event_consumer_put` (the contract's `ConsumerSpec`), shape-checked by the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerSpecIn {
    /// Exact topic names or a prefix ending in `.*`; 1 to 64, pattern-checked by the route.
    pub topics: Vec<String>,
    /// An optional filter expression, at most 2000 characters; parsed by the port.
    pub filter: Option<String>,
    /// 1 to 64, range-checked by the route.
    pub partitions: u32,
    /// How much of each event to deliver.
    pub payload_mode: PayloadModeIn,
    /// Where it starts; `None` means `tail`.
    pub start: Option<StartIn>,
}

/// One partition's state (the contract's `partitions_state` item).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionOut {
    /// The partition.
    pub partition: u32,
    /// Its checkpoint, as an opaque position.
    pub checkpoint: String,
    /// True when the partition made no progress for the park timeout.
    pub parked: bool,
}

/// A consumer as the contract's `EventConsumer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerOut {
    /// The consumer's name.
    pub name: String,
    /// The owner's record id.
    pub owner: String,
    /// The topic selectors.
    pub topics: Vec<String>,
    /// The filter expression, when there is one.
    pub filter: Option<String>,
    /// The partition count.
    pub partitions: u32,
    /// The payload mode.
    pub payload_mode: PayloadModeIn,
    /// Where each partition stands.
    pub partitions_state: Vec<PartitionOut>,
}

/// The body of `event_consumer_pull` after the route's defaults (`limit` 100, `wait_ms` 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PullIn {
    /// The partition to read.
    pub partition: u32,
    /// 1 to 1000.
    pub limit: u32,
    /// 0 to 30000; how long the call may wait for events when there are none.
    pub wait_ms: u32,
}

/// A pull's answer (the contract's `PullBatch` and the `X-Consumer-Lease` header).
#[derive(Clone, Debug, PartialEq)]
pub struct PullOut {
    /// The lease to send back as `X-Consumer-Lease`.
    pub lease: String,
    /// The events, as `EventEnvelope` JSON, in order within the partition.
    pub events: Vec<Json>,
    /// The position to acknowledge once every returned event is applied.
    pub next_position: String,
    /// Events in the range the owner may not read; names no subject.
    pub skipped: u64,
    /// True when the consumer already has its in-flight window of unacknowledged events.
    pub window_full: bool,
}

/// A partition's checkpoint (the contract's `Checkpoint`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointOut {
    /// The partition.
    pub partition: u32,
    /// Its checkpoint, as an opaque position.
    pub position: String,
}

/// The query of `event_stream`, already merged by the route: `after` is `Last-Event-ID` when
/// the header is sent (it is preferred), else the `after` query parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamIn {
    /// The consumer's name.
    pub consumer: String,
    /// The partition to follow.
    pub partition: u32,
    /// The position to resume after, when the client has one.
    pub after: Option<String>,
}

/// The timing the route applies to an open stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    /// A `: keepalive` comment goes out when nothing was sent for this long (the contract: 25 s).
    pub heartbeat: std::time::Duration,
    /// The stream ends when no event was delivered for this long (heartbeats do not count).
    pub max_idle: std::time::Duration,
}

/// One step of an open stream.
#[derive(Debug, PartialEq, Eq)]
pub enum FeedItem {
    /// Bytes to write as they are: one or more whole SSE frames.
    Frame(Vec<u8>),
    /// Nothing to send now. The feed returned after its own poll interval so the route can
    /// check the heartbeat and the idle limit.
    Idle,
    /// The stream is over: the credential ended, the lease was lost, the server is stopping,
    /// or a `resync_required` frame was the last thing sent.
    Closed,
}

/// An open `event_stream`: it holds the consumer's lease until it is dropped.
pub trait EventFeed: Send {
    /// The lease to answer in `X-Consumer-Lease`.
    fn lease(&self) -> &str;

    /// The next step. It must return within the heartbeat interval (as `Idle` when it has
    /// nothing), must keep its own state across calls, and must not lose an event it already
    /// took from the engine: the route never cancels this future.
    fn next(&mut self) -> BoxFuture<'_, FeedItem>;
}

/// The event stream behind the routes. Every method answers for the caller `ctx.credential`
/// names on `ctx.site`; a consumer of another owner or another site is `404
/// eventstream.consumer_not_found`, the same answer as one that does not exist.
pub trait EventStreamPort: Send + Sync + 'static {
    /// `event_consumer_list`: the consumers the caller owns (a tenant administrator: all).
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, EventsResult<Vec<ConsumerOut>>>;

    /// `event_consumer_put`: creates the consumer (`true`) or answers the existing identical
    /// one (`false`). The caller becomes the owner.
    fn put<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        spec: ConsumerSpecIn,
    ) -> BoxFuture<'a, EventsResult<(ConsumerOut, bool)>>;

    /// `event_consumer_get`.
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, EventsResult<ConsumerOut>>;

    /// `event_consumer_delete`.
    fn delete<'a>(&'a self, ctx: &'a RequestCtx, name: &'a str)
    -> BoxFuture<'a, EventsResult<()>>;

    /// `event_consumer_pull`. `lease` is the `X-Consumer-Lease` header when sent; without it the
    /// port takes the lease or answers `409 eventstream.consumer_busy`.
    fn pull<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        request: PullIn,
    ) -> BoxFuture<'a, EventsResult<PullOut>>;

    /// `event_consumer_ack`.
    fn ack<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        position: &'a str,
    ) -> BoxFuture<'a, EventsResult<CheckpointOut>>;

    /// `event_consumer_restart`.
    fn restart<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        from: StartIn,
    ) -> BoxFuture<'a, EventsResult<CheckpointOut>>;

    /// `event_stream`: takes the consumer's lease and returns the open feed, or the error to
    /// answer with (`409 eventstream.consumer_busy`, `429 api.rate_limited` over the connection
    /// bound, ...).
    fn open_stream<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: StreamIn,
    ) -> BoxFuture<'a, EventsResult<Box<dyn EventFeed>>>;

    /// The heartbeat and idle limits of an open stream.
    fn limits(&self) -> StreamLimits;
}
