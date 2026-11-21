//! Synchronous bounded queue. Producers never wait for a contended queue lock.
//! Drops are counted cumulatively and sent to an injected counts-only telemetry seam.
use crate::{MeterEvent, MeteringError};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

/// A counts-only bridge to the host's telemetry recorder. Implementations must not
/// block, read a clock, or attach tenant/member/event identifiers as metric labels.
pub trait MeterTelemetry: Send + Sync {
    /// Increment the counter named `metering.dropped` by `count`.
    fn dropped(&self, count: u64);
}

/// Why a queue declined an observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkDropReason {
    /// The event payload disagreed with its meter kind.
    Malformed,
    /// Capacity exhausted.
    QueueFull,
    /// Another producer or the flusher owns the queue; retry the same event id.
    Contended,
}
/// Producer receipt; persistence and deduplication happen at the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkReceipt {
    /// Buffered, not yet durably stored.
    Queued,
    /// Not buffered; retry from the producer's durable source.
    Dropped(SinkDropReason),
}
impl SinkReceipt {
    /// Whether the event was buffered.
    #[must_use]
    pub fn is_queued(&self) -> bool {
        matches!(self, Self::Queued)
    }
}
/// Producers report synchronously; the host drains and persists batches.
pub trait MeterSink: Send + Sync {
    /// No waiting for queue capacity or a queue lock. Malformed events return an error.
    fn report(&self, event: MeterEvent) -> Result<SinkReceipt, MeteringError>;
    /// Drain at most `limit` events. The flusher may take a lock.
    fn drain(&self, limit: usize) -> Vec<MeterEvent>;
    /// Cumulative drops, including contention and malformed observations.
    fn dropped(&self) -> u64;
}
/// Bounded in-process buffer, with preallocated slots and an atomic loss counter.
pub struct BoundedSink {
    queue: Mutex<VecDeque<MeterEvent>>,
    capacity: usize,
    dropped: AtomicU64,
    telemetry: Option<Arc<dyn MeterTelemetry>>,
}
impl std::fmt::Debug for BoundedSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedSink")
            .field("capacity", &self.capacity)
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}
impl BoundedSink {
    /// Buffer at most `capacity` events. Read the atomic drop count for reconciliation.
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        Self::with_telemetry(capacity, None)
    }
    /// Connect a counts-only telemetry recorder without depending on its backend.
    #[must_use]
    pub fn with_telemetry(
        capacity: usize,
        telemetry: Option<Arc<dyn MeterTelemetry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            dropped: AtomicU64::new(0),
            telemetry,
        })
    }
    /// Configured slot count.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    /// Buffered events (management-only; may lock).
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
    /// Whether the buffer is empty (management-only).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn count_drop(&self) {
        let _ = self
            .dropped
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
        if let Some(telemetry) = &self.telemetry {
            telemetry.dropped(1);
        }
    }
}
impl MeterSink for BoundedSink {
    fn report(&self, event: MeterEvent) -> Result<SinkReceipt, MeteringError> {
        if !event.is_well_formed() {
            self.count_drop();
            return Err(MeteringError::MalformedEvent {
                id: event.id,
                meter: event.meter.to_string(),
            });
        }
        let mut queue = match self.queue.try_lock() {
            Ok(queue) => queue,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.count_drop();
                return Ok(SinkReceipt::Dropped(SinkDropReason::Contended));
            }
        };
        if queue.len() >= self.capacity {
            drop(queue);
            self.count_drop();
            return Ok(SinkReceipt::Dropped(SinkDropReason::QueueFull));
        }
        queue.push_back(event);
        Ok(SinkReceipt::Queued)
    }
    fn drain(&self, limit: usize) -> Vec<MeterEvent> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let count = limit.min(queue.len());
        queue.drain(..count).collect()
    }
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::meter::{MemberDigest, Meter};

    fn counter_event(id: &str) -> MeterEvent {
        MeterEvent::amount(id, "acme", Meter::Transactions, 1, 0)
    }

    #[test]
    fn reporting_buffers_and_draining_returns_in_arrival_order() {
        let sink = BoundedSink::new(8);
        for id in ["a", "b", "c"] {
            assert_eq!(sink.report(counter_event(id)).unwrap(), SinkReceipt::Queued);
        }
        assert_eq!(sink.len(), 3);
        let drained: Vec<_> = sink.drain(10).iter().map(|e| e.id.clone()).collect();
        assert_eq!(drained, ["a", "b", "c"]);
        assert!(sink.is_empty());
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn a_full_queue_drops_non_blocking_and_counts() {
        let sink = BoundedSink::new(2);
        assert!(sink.report(counter_event("a")).unwrap().is_queued());
        assert!(sink.report(counter_event("b")).unwrap().is_queued());
        assert_eq!(
            sink.report(counter_event("c")).unwrap(),
            SinkReceipt::Dropped(SinkDropReason::QueueFull)
        );
        assert_eq!(sink.dropped(), 1);
        assert_eq!(sink.len(), 2);
        // Draining frees capacity but the drop count is cumulative, never reset.
        sink.drain(2);
        assert!(sink.report(counter_event("d")).unwrap().is_queued());
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn drain_respects_the_batch_limit() {
        let sink = BoundedSink::new(8);
        for id in ["a", "b", "c", "d"] {
            sink.report(counter_event(id)).unwrap();
        }
        assert_eq!(sink.drain(2).len(), 2);
        assert_eq!(sink.len(), 2);
    }

    #[test]
    fn a_malformed_event_is_refused_and_counted() {
        let sink = BoundedSink::new(4);
        // A distinct-set meter fed a plain amount is malformed.
        let bad = MeterEvent::amount("s1", "acme", Meter::ActiveUsers, 1, 0);
        let err = sink.report(bad).unwrap_err();
        assert_eq!(err.code(), "metering.malformed_event");
        assert_eq!(sink.dropped(), 1);
        assert!(sink.is_empty());

        // A well-formed member event is accepted.
        let good = MeterEvent::member("s2", "acme", Meter::ActiveUsers, MemberDigest::new("h"), 0);
        assert!(sink.report(good).unwrap().is_queued());
    }
    #[test]
    fn contended_producer_never_waits_and_emits_count_only_telemetry() {
        #[derive(Default)]
        struct Counts(AtomicU64);
        impl MeterTelemetry for Counts {
            fn dropped(&self, count: u64) {
                self.0.fetch_add(count, Ordering::Relaxed);
            }
        }
        let counts = Arc::new(Counts::default());
        let sink = BoundedSink::with_telemetry(1, Some(counts.clone()));
        let held = sink.queue.lock().unwrap();
        assert_eq!(
            sink.report(counter_event("contended")).unwrap(),
            SinkReceipt::Dropped(SinkDropReason::Contended)
        );
        drop(held);
        sink.report(counter_event("queued")).unwrap();
        sink.report(counter_event("full")).unwrap();
        assert_eq!(sink.dropped(), 2);
        assert_eq!(counts.0.load(Ordering::Relaxed), 2);
    }
}
