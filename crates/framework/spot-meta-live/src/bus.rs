//! The [`InvalidationBus`] interface and its single-node default, [`InProcessBus`].
//!
//! The bus is how a metadata change reaches every node (blueprint, cache and coordination,
//! ADR-20). [`InProcessBus`] fans out in one process — the default for standalone and a single
//! server; [`PollingBus`](crate::PollingBus) is the multi-node backend a database serves. The
//! trait is object-safe (`Arc<dyn InvalidationBus>`) so WP-PLT-19's Redis backend implements
//! the very same interface.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use futures_core::Stream;
use spot_core::SiteId;
use tokio::sync::mpsc;

use crate::error::LiveResult;
use crate::invalidation::{BusEvent, Invalidation};

/// The future a [`InvalidationBus::publish`] returns. The trait stays object-safe, so async
/// work is erased behind a boxed future.
pub type BusFuture<'a, T> = Pin<Box<dyn Future<Output = LiveResult<T>> + Send + 'a>>;

/// A subscription's stream: the host drives it with [`next_event`]. The crate spawns nothing
/// and holds no timer.
pub type InvalidationStream = Pin<Box<dyn Stream<Item = BusEvent> + Send>>;

/// Default per-subscription queue: it comfortably holds 1,000 rapid changes before a subscriber
/// that never keeps up is reported as lagging with [`BusEvent::Gap`].
pub const DEFAULT_BUS_CAPACITY: usize = 4096;

/// Tells every node that a site's metadata (or, later, cache keys) changed.
///
/// [`publish`](InvalidationBus::publish) never blocks a caller on a subscriber; the polling
/// backend additionally persists the change so other nodes can read it. A subscriber that
/// falls behind reads [`BusEvent::Gap`] and reconciles from the version store.
pub trait InvalidationBus: Send + Sync {
    /// Announces `invalidation` for `site`. Completes once the change is durable enough for
    /// this backend: an in-process broadcast has handed it to every current subscriber, a
    /// polling backend has written it to the version store.
    fn publish<'a>(&'a self, site: &'a SiteId, invalidation: Invalidation) -> BusFuture<'a, ()>;

    /// Subscribes to `site`. Only changes published after this call are delivered.
    fn subscribe(&self, site: &SiteId) -> InvalidationStream;
}

/// The next event from `stream`, or `None` when it ended. The host loop awaits this; nothing in
/// this crate polls a stream on its own.
pub async fn next_event(stream: &mut InvalidationStream) -> Option<BusEvent> {
    std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

/// Per-site subscriber list. A publish hands the change to each subscriber with a non-blocking
/// `try_send`; a full queue marks that subscriber lagging rather than waiting for it, exactly
/// like the realtime bus's bounded delivery.
#[derive(Default)]
struct SiteChannels {
    next_id: u64,
    subscribers: Vec<Subscriber>,
}

struct Subscriber {
    id: u64,
    tx: mpsc::Sender<BusEvent>,
    lagged: Arc<AtomicBool>,
}

impl SiteChannels {
    /// Fans `event` out; a full queue sets the subscriber's lag flag, a closed one is dropped.
    fn broadcast(&mut self, event: BusEvent) {
        let mut closed: Vec<u64> = Vec::new();
        for subscriber in &self.subscribers {
            match subscriber.tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    subscriber.lagged.store(true, Ordering::SeqCst);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => closed.push(subscriber.id),
            }
        }
        if !closed.is_empty() {
            self.subscribers
                .retain(|subscriber| !closed.contains(&subscriber.id));
        }
    }
}

/// The single-node bus: one bounded queue per subscriber, filled without ever blocking.
///
/// A publish hands the change to every current subscriber without awaiting. A subscriber whose
/// queue overflows is not disconnected (unlike the realtime bus's slow-consumer policy) — it
/// reads [`BusEvent::Gap`] on its next receive and reconciles from the store, because a
/// metadata snapshot can always be reloaded.
pub struct InProcessBus {
    capacity: usize,
    sites: Mutex<HashMap<SiteId, SiteChannels>>,
}

impl InProcessBus {
    /// A bus whose per-subscription queue holds `capacity` changes (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            sites: Mutex::new(HashMap::new()),
        }
    }

    /// The number of live subscribers on `site`.
    #[must_use]
    pub fn subscribers(&self, site: &SiteId) -> usize {
        self.lock()
            .get(site)
            .map_or(0, |channels| channels.subscribers.len())
    }

    /// Fans `invalidation` out to `site`'s subscribers. Used by
    /// [`PollingBus`](crate::PollingBus) too, so a polling deployment still gets same-process
    /// push delivery.
    pub(crate) fn send(&self, site: &SiteId, invalidation: Invalidation) {
        let mut sites = self.lock();
        if let Some(channels) = sites.get_mut(site) {
            channels.broadcast(BusEvent::Change(invalidation));
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SiteId, SiteChannels>> {
        // The guarded data is a plain map of subscriber lists; a panic elsewhere cannot make it
        // inconsistent, and no `.await` ever happens under this lock.
        self.sites
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for InProcessBus {
    fn default() -> Self {
        Self::new(DEFAULT_BUS_CAPACITY)
    }
}

impl std::fmt::Debug for InProcessBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessBus")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl InvalidationBus for InProcessBus {
    fn publish<'a>(&'a self, site: &'a SiteId, invalidation: Invalidation) -> BusFuture<'a, ()> {
        self.send(site, invalidation);
        Box::pin(async { Ok(()) })
    }

    fn subscribe(&self, site: &SiteId) -> InvalidationStream {
        let lagged = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(self.capacity);
        let mut sites = self.lock();
        let channels = sites.entry(site.clone()).or_default();
        let id = channels.next_id;
        channels.next_id += 1;
        channels.subscribers.push(Subscriber {
            id,
            tx,
            lagged: Arc::clone(&lagged),
        });
        drop(sites);
        Box::pin(SiteStream { rx, lagged })
    }
}

/// One subscriber's queue, as a [`Stream`]. A publish that overflowed the queue is reported
/// once as [`BusEvent::Gap`] before the buffered changes, so the host reconciles.
struct SiteStream {
    rx: mpsc::Receiver<BusEvent>,
    lagged: Arc<AtomicBool>,
}

impl Stream for SiteStream {
    type Item = BusEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BusEvent>> {
        let this = self.get_mut();
        if this.lagged.swap(false, Ordering::SeqCst) {
            return Poll::Ready(Some(BusEvent::Gap));
        }
        this.rx.poll_recv(cx)
    }
}
