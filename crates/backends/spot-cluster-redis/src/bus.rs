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

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use spot_core::SiteId;
use spot_meta_live::{
    BusEvent, BusFuture, Invalidation, InvalidationBus, InvalidationKind, InvalidationStream,
    LiveError, MetaHash, Version,
};
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
            },
            // `Meta`, and any future kind whose payload is just the version and hash.
            _ => Self {
                kind: "meta".to_owned(),
                version: invalidation.version.get(),
                hash: invalidation.hash.as_hex().to_owned(),
                keys: Vec::new(),
            },
        }
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

impl InvalidationBus for RedisBus {
    fn publish<'a>(&'a self, site: &'a SiteId, invalidation: Invalidation) -> BusFuture<'a, ()> {
        Box::pin(async move {
            let payload =
                serde_json::to_string(&BusMessage::of(&invalidation)).map_err(|error| {
                    LiveError::Store {
                        message: format!("encoding the bus message: {error}"),
                    }
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

    fn subscribe(&self, site: &SiteId) -> InvalidationStream {
        let lagged = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(self.capacity);
        let channel = self.channel(site);
        let client = self.connection.client();
        let flag = Arc::clone(&lagged);
        // The trait's `subscribe` is synchronous; the reader needs a runtime. Without one the
        // stream simply ends (a host that subscribes always has one).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                run_reader(client, channel, tx, flag).await;
            });
        }
        Box::pin(ChannelStream { rx, lagged })
    }
}

/// Owns one pub/sub connection: subscribes, forwards messages, and reconnects with backoff.
async fn run_reader(
    client: redis::Client,
    channel: String,
    tx: mpsc::Sender<BusEvent>,
    lagged: Arc<AtomicBool>,
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
            lagged.store(true, Ordering::SeqCst);
        }
        first = false;
        let mut messages = pubsub.on_message();
        while let Some(message) = messages.next().await {
            let payload: String = match message.get_payload() {
                Ok(payload) => payload,
                Err(_) => {
                    lagged.store(true, Ordering::SeqCst);
                    continue;
                }
            };
            match serde_json::from_str::<BusMessage>(&payload)
                .map_err(|error| LiveError::Store {
                    message: error.to_string(),
                })
                .and_then(BusMessage::into_invalidation)
            {
                Ok(invalidation) => {
                    if tx.try_send(BusEvent::Change(invalidation)).is_err() {
                        lagged.store(true, Ordering::SeqCst);
                    }
                }
                Err(_) => lagged.store(true, Ordering::SeqCst),
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
    rx: mpsc::Receiver<BusEvent>,
    lagged: Arc<AtomicBool>,
}

impl Stream for ChannelStream {
    type Item = BusEvent;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BusEvent>> {
        let this = self.get_mut();
        if this.lagged.swap(false, Ordering::SeqCst) {
            return Poll::Ready(Some(BusEvent::Gap));
        }
        this.rx.poll_recv(cx)
    }
}
