//! [`RedisBus`]: the [`InvalidationBus`] over Redis/Valkey pub/sub, one channel per tenant.
//!
//! [`publish`](InvalidationBus::publish) serialises an [`Invalidation`] into a small JSON envelope
//! ([`BusMessage`]) and `PUBLISH`es it on `{prefix}:bus:{site}`. A subscriber holds a bounded
//! queue fed by a reader task that owns its own pub/sub connection; if the queue overflows the
//! subscriber reads [`BusEvent::Gap`] on its next receive and reconciles from the version store,
//! exactly like the in-process bus. When the reader's connection drops it reconnects with backoff,
//! resubscribes and signals a **full flush** as a [`Gap`](BusEvent::Gap) — a message published
//! while it was away is gone (pub/sub has no replay), so the consumer must reload, not trust the
//! stream to be gap-free. Delivery is therefore at-least-once for a consumer that treats a `Gap`
//! as "reload everything", which is the contract's model.
//!
//! # Message shape
//!
//! ```json
//! { "kind": "meta",  "version": 7, "hash": "ab.." }
//! { "kind": "cache", "version": 7, "hash": "ab..", "keys": ["meta:x"] }
//! ```
//!
//! The envelope is versioned by the `{prefix}` in the channel, not by a field: two plugin versions
//! with different prefixes never share a channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::task::AtomicWaker;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use spot_core::SiteId;
use spot_meta_live::{
    BusEvent, BusFuture, Invalidation, InvalidationBus, InvalidationKind, InvalidationStream,
    LiveError, MetaHash, Version,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;

use crate::connection::RedisConnection;

/// Default per-subscription queue length, matching the in-process bus.
pub const SUBSCRIBER_CAPACITY: usize = 4096;

/// The first reconnect delay; each later retry doubles it, bounded by [`MAX_RECONNECT_BACKOFF`].
pub const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// The longest reconnect delay.
pub const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// One invalidation on the wire: a metadata change or a cache-key change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusMessage {
    /// `meta` or `cache`.
    pub kind: String,
    /// The metadata version the message refers to (`0` for a pure cache message).
    pub version: u64,
    /// The metadata content hash (`""` for a pure cache message).
    pub hash: String,
    /// The keys a cache message invalidates; empty for a metadata change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// When the publisher sent the message, in milliseconds since the Unix epoch, from the
    /// publisher's injected clock. Absent from an older node's message (still accepted) and from a
    /// publish made without a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at_ms: Option<i64>,
}

/// The wire form of a publish time (milliseconds since the Unix epoch).
fn unix_ms(at: OffsetDateTime) -> Option<i64> {
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

/// One event of a [`RedisBus::subscribe_stamped`] stream: the event and when its publisher sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampedEvent {
    /// The event, exactly what [`InvalidationBus::subscribe`] would have yielded.
    pub event: BusEvent,
    /// The publisher's send time in milliseconds since the Unix epoch; `None` for a gap, a message
    /// from an older node or a publish made without a time.
    pub sent_at_ms: Option<i64>,
}

impl StampedEvent {
    /// How long the message was in flight, given the receiver's `now` from its own `Clock`.
    ///
    /// `None` when the message carries no publish time. Never negative: clock skew between nodes
    /// that puts the send time after `now` reads as zero.
    #[must_use]
    pub fn lag(&self, now: OffsetDateTime) -> Option<Duration> {
        let sent = self.sent_at_ms?;
        let now_ms = unix_ms(now)?;
        Some(Duration::from_millis(
            u64::try_from(now_ms.saturating_sub(sent)).unwrap_or(0),
        ))
    }
}

impl BusMessage {
    /// Encodes `invalidation` as a wire message.
    #[must_use]
    pub fn of(invalidation: &Invalidation) -> Self {
        match &invalidation.kind {
            InvalidationKind::Cache { keys } => Self {
                kind: "cache".to_owned(),
                version: invalidation.version.get(),
                hash: invalidation.hash.as_hex().to_owned(),
                keys: keys.clone(),
                sent_at_ms: None,
            },
            // `Meta`, and any future kind whose payload is just the version and hash.
            _ => Self {
                kind: "meta".to_owned(),
                version: invalidation.version.get(),
                hash: invalidation.hash.as_hex().to_owned(),
                keys: Vec::new(),
                sent_at_ms: None,
            },
        }
    }

    /// The same message stamped with the publisher's time.
    #[must_use]
    pub fn sent_at(mut self, at: OffsetDateTime) -> Self {
        self.sent_at_ms = unix_ms(at);
        self
    }

    /// Decodes the wire message back into an [`Invalidation`].
    ///
    /// # Errors
    ///
    /// [`LiveError::Store`] when the kind is unknown.
    pub fn into_invalidation(self) -> Result<Invalidation, LiveError> {
        let version = Version::new(self.version);
        let hash = MetaHash::from_hex(self.hash);
        match self.kind.as_str() {
            "meta" => Ok(Invalidation::meta(version, hash)),
            "cache" => Ok(Invalidation::cache(version, hash, self.keys)),
            other => Err(LiveError::Store {
                message: format!("unknown bus message kind {other:?}"),
            }),
        }
    }
}

/// The multi-node [`InvalidationBus`] over Redis/Valkey pub/sub.
#[derive(Clone, Debug)]
pub struct RedisBus {
    connection: RedisConnection,
    capacity: usize,
}

impl RedisBus {
    /// A bus with the default per-subscription queue length.
    #[must_use]
    pub fn new(connection: RedisConnection) -> Self {
        Self {
            connection,
            capacity: SUBSCRIBER_CAPACITY,
        }
    }

    /// A bus whose per-subscription queue holds `capacity` events (at least one).
    #[must_use]
    pub fn with_capacity(connection: RedisConnection, capacity: usize) -> Self {
        Self {
            connection,
            capacity: capacity.max(1),
        }
    }

    /// The channel one site's changes travel on.
    #[must_use]
    pub fn channel(&self, site: &SiteId) -> String {
        self.connection.keyspace().bus_channel(site)
    }
}

impl RedisBus {
    /// Publishes like [`InvalidationBus::publish`], and stamps the message with `now`, the
    /// caller's `Clock` reading, so a subscriber of [`subscribe_stamped`](Self::subscribe_stamped)
    /// can measure the receive lag.
    pub fn publish_at<'a>(
        &'a self,
        site: &'a SiteId,
        invalidation: Invalidation,
        now: OffsetDateTime,
    ) -> BusFuture<'a, ()> {
        self.send(site, BusMessage::of(&invalidation).sent_at(now))
    }

    /// Subscribes like [`InvalidationBus::subscribe`], keeping each message's publish time so the
    /// consumer can compute the lag with [`StampedEvent::lag`].
    #[must_use]
    pub fn subscribe_stamped(
        &self,
        site: &SiteId,
    ) -> std::pin::Pin<Box<dyn Stream<Item = StampedEvent> + Send>> {
        let flag = Arc::new(LagFlag::default());
        let (tx, rx) = mpsc::channel(self.capacity);
        let channel = self.channel(site);
        let client = self.connection.client();
        let reader_flag = Arc::clone(&flag);
        // The trait's `subscribe` is synchronous; the reader needs a runtime. Without one the
        // stream simply ends (a host that subscribes always has one).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                run_reader(client, channel, tx, reader_flag).await;
            });
        }
        Box::pin(ChannelStream { rx, flag })
    }

    fn send<'a>(&'a self, site: &'a SiteId, message: BusMessage) -> BusFuture<'a, ()> {
        Box::pin(async move {
            let payload = serde_json::to_string(&message).map_err(|error| LiveError::Store {
                message: format!("encoding the bus message: {error}"),
            })?;
            let mut manager = self.connection.manager();
            redis::cmd("PUBLISH")
                .arg(self.channel(site))
                .arg(payload)
                .query_async::<i64>(&mut manager)
                .await
                .map_err(|error| LiveError::Store {
                    message: format!("publishing to {}: {error}", self.channel(site)),
                })?;
            Ok(())
        })
    }
}

impl InvalidationBus for RedisBus {
    /// Publishes without a time (the trait carries no clock); use [`RedisBus::publish_at`] to
    /// stamp the message.
    fn publish<'a>(&'a self, site: &'a SiteId, invalidation: Invalidation) -> BusFuture<'a, ()> {
        self.send(site, BusMessage::of(&invalidation))
    }

    fn subscribe(&self, site: &SiteId) -> InvalidationStream {
        Box::pin(self.subscribe_stamped(site).map(|stamped| stamped.event))
    }
}

/// "A message was lost", plus the waker of the stream that must report it.
#[derive(Default)]
struct LagFlag {
    lagged: AtomicBool,
    waker: AtomicWaker,
}

impl LagFlag {
    /// Marks a loss and wakes the stream, so a quiet subscriber sees the `Gap` at once.
    fn raise(&self) {
        self.lagged.store(true, Ordering::SeqCst);
        self.waker.wake();
    }
}

/// Owns one pub/sub connection: subscribes, forwards messages, and reconnects with backoff.
async fn run_reader(
    client: redis::Client,
    channel: String,
    tx: mpsc::Sender<StampedEvent>,
    lagged: Arc<LagFlag>,
) {
    let mut delay = RECONNECT_BACKOFF;
    let mut first = true;
    loop {
        if tx.is_closed() {
            return;
        }
        let Ok(mut pubsub) = client.get_async_pubsub().await else {
            back_off(&mut delay).await;
            continue;
        };
        if pubsub.subscribe(&channel).await.is_err() {
            back_off(&mut delay).await;
            continue;
        }
        delay = RECONNECT_BACKOFF;
        if !first {
            // Messages published while the connection was down are lost: the consumer must
            // reload from the version store.
            lagged.raise();
        }
        first = false;
        let mut messages = pubsub.on_message();
        while let Some(message) = messages.next().await {
            let payload: String = match message.get_payload() {
                Ok(payload) => payload,
                Err(_) => {
                    lagged.raise();
                    continue;
                }
            };
            match serde_json::from_str::<BusMessage>(&payload)
                .map_err(|error| LiveError::Store {
                    message: error.to_string(),
                })
                .and_then(|message| {
                    let sent_at_ms = message.sent_at_ms;
                    message.into_invalidation().map(|inv| (inv, sent_at_ms))
                }) {
                Ok((invalidation, sent_at_ms)) => {
                    let stamped = StampedEvent {
                        event: BusEvent::Change(invalidation),
                        sent_at_ms,
                    };
                    if tx.try_send(stamped).is_err() {
                        lagged.raise();
                    }
                }
                Err(_) => lagged.raise(),
            }
        }
        back_off(&mut delay).await;
    }
}

/// Waits `delay` and doubles it up to [`MAX_RECONNECT_BACKOFF`].
async fn back_off(delay: &mut Duration) {
    tokio::time::sleep(*delay).await;
    *delay = (*delay * 2).min(MAX_RECONNECT_BACKOFF);
}

/// One subscriber's queue, as a [`Stream`]. A lost message is reported once as [`BusEvent::Gap`].
struct ChannelStream {
    rx: mpsc::Receiver<StampedEvent>,
    flag: Arc<LagFlag>,
}

impl Stream for ChannelStream {
    type Item = StampedEvent;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<StampedEvent>> {
        let this = self.get_mut();
        // Register before checking, so a loss raised in between still wakes this task.
        this.flag.waker.register(cx.waker());
        if this.flag.lagged.swap(false, Ordering::SeqCst) {
            return Poll::Ready(Some(StampedEvent {
                event: BusEvent::Gap,
                sent_at_ms: None,
            }));
        }
        this.rx.poll_recv(cx)
    }
}
