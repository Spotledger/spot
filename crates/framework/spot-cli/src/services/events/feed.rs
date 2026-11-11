//! An open `event_stream`: the feed the route pulls frame by frame.

use super::*;

/// One open stream's place in the connection budget.
pub(super) struct StreamSlot {
    pub(super) shared: Arc<Shared>,
    pub(super) user: String,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut streams = lock(&self.shared.streams);
        streams.total = streams.total.saturating_sub(1);
        if let Some(count) = streams.per_user.get_mut(&self.user) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                streams.per_user.remove(&self.user);
            }
        }
    }
}

/// An open `event_stream`: it holds the consumer's lease, renews it, asks again whether the
/// credential still holds, and gives the lease back when it is dropped.
pub(super) struct StreamFeed {
    pub(super) shared: Arc<Shared>,
    pub(super) ctx: RequestCtx,
    pub(super) consumer: String,
    pub(super) partition: u32,
    pub(super) lease: ConsumerLease,
    pub(super) token: String,
    /// Events at or before this position were applied by the client already.
    pub(super) after: Option<Position>,
    /// A lost resume position: the retained checkpoint to name in the `resync_required` frame.
    pub(super) resync: Option<Position>,
    /// Frames taken from the engine and not yet handed to the route.
    pub(super) queued: VecDeque<Vec<u8>>,
    pub(super) revalidated: tokio::time::Instant,
    pub(super) renewed: tokio::time::Instant,
    pub(super) stop: watch::Receiver<bool>,
    pub(super) closed: bool,
    pub(super) _slot: StreamSlot,
}

pub(super) fn event_frame(consumer: &str, envelope: &EventEnvelope) -> Vec<u8> {
    // `serde_json` escapes every control character, so the data is one line whatever the
    // payload holds.
    let data = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".to_owned());
    format!(
        "id: {}\nevent: {}\ndata: {data}\n\n",
        wire(consumer, envelope.position),
        envelope.topic.as_str()
    )
    .into_bytes()
}

pub(super) fn resync_frame(consumer: &str, retained_from: Position) -> Vec<u8> {
    let data = json!({ "retained_from": wire(consumer, retained_from) });
    format!("event: resync_required\ndata: {data}\n\n").into_bytes()
}

impl StreamFeed {
    pub(super) async fn pause(&mut self) -> FeedItem {
        let poll = self.shared.poll();
        tokio::select! {
            () = tokio::time::sleep(poll) => {}
            _ = self.stop.changed() => {}
        }
        FeedItem::Idle
    }

    /// Whether the credential, the rights and the lease still hold (every few seconds).
    pub(super) async fn still_allowed(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        let settings = &self.shared.settings;
        if now.duration_since(self.renewed) >= self.shared.lease_ttl() / 3 {
            match self
                .shared
                .coordinator
                .renew(&self.lease.lease, self.shared.lease_ttl())
                .await
            {
                Ok(renewed) => {
                    self.lease.lease = renewed.clone();
                    if let Some(entry) = lock(&self.shared.held).get_mut(&self.consumer) {
                        entry.lease = renewed;
                    }
                    self.renewed = now;
                }
                Err(_) => return false,
            }
        }
        if now.duration_since(self.revalidated) < Duration::from_secs(settings.revalidate_seconds) {
            return true;
        }
        self.revalidated = now;
        let Ok(caller) = self.shared.caller(&self.ctx).await else {
            return false;
        };
        let Ok(record) = self.shared.visible(&caller, &self.consumer).await else {
            return false;
        };
        let owner = self.shared.refresh_owner(&record.spec.owner).await;
        self.shared.check_owner_rights(&record.spec, &owner).is_ok()
    }

    pub(super) async fn step(&mut self) -> FeedItem {
        if self.closed || *self.stop.borrow() {
            return FeedItem::Closed;
        }
        if let Some(retained) = self.resync.take() {
            self.closed = true;
            return FeedItem::Frame(resync_frame(&self.consumer, retained));
        }
        if let Some(frame) = self.queued.pop_front() {
            return FeedItem::Frame(frame);
        }
        if !self.still_allowed().await {
            return FeedItem::Closed;
        }
        let limit = u32::try_from(self.shared.settings.window.min(500)).unwrap_or(500);
        let batch = match self
            .shared
            .pull_once(&self.lease, self.partition, limit, false)
            .await
        {
            Ok(batch) => batch,
            Err(error) => {
                return match error.code.as_str() {
                    "eventstream.resync_required" => {
                        let retained = self
                            .shared
                            .engine
                            .checkpoint(&self.consumer, self.partition)
                            .await
                            .unwrap_or(Position {
                                generation: 0,
                                seq: 0,
                            });
                        self.closed = true;
                        FeedItem::Frame(resync_frame(&self.consumer, retained))
                    }
                    "eventstream.consumer_not_found" | "eventstream.consumer_busy" => {
                        FeedItem::Closed
                    }
                    code => {
                        tracing::warn!(code, "the event stream could not read the feed");
                        self.pause().await
                    }
                };
            }
        };
        let mut bytes: Vec<u8> = Vec::new();
        for envelope in &batch.events {
            if self.after.is_some_and(|a| order(envelope.position) <= order(a)) {
                continue;
            }
            bytes.extend(event_frame(&self.consumer, envelope));
        }
        if let Some(last) = batch.events.last() {
            self.after = Some(match self.after {
                Some(a) if order(a) > order(last.position) => a,
                _ => last.position,
            });
        }
        if bytes.is_empty() {
            return self.pause().await;
        }
        FeedItem::Frame(bytes)
    }
}

impl EventFeed for StreamFeed {
    fn lease(&self) -> &str {
        &self.token
    }

    fn next(&mut self) -> BoxFuture<'_, FeedItem> {
        Box::pin(self.step())
    }
}

impl Drop for StreamFeed {
    fn drop(&mut self) {
        let shared = Arc::clone(&self.shared);
        let consumer = self.consumer.clone();
        let token = self.token.clone();
        let lease = self.lease.lease.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime to give the lease back on: it lapses with its time to live.
            return;
        };
        handle.spawn(async move {
            let ours = {
                let mut held = lock(&shared.held);
                if held.get(&consumer).is_some_and(|h| h.token == token) {
                    held.remove(&consumer);
                    true
                } else {
                    false
                }
            };
            if ours {
                let _ = shared.coordinator.release(&lease).await;
            }
        });
    }
}

