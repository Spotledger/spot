//! Bounds and counters of the WebSocket route (WP-PLT-08.T7).
//!
//! - [`ConnectionGate`] caps the sockets one route holds open. It counts from the moment the
//!   upgrade is accepted, so a socket that never sends `auth` counts too (it is not a bus
//!   subscription, so `BusStats::subscriptions` cannot see it); [`ConnectionGuard`] gives the
//!   slot back on every exit path, a panicking session task and a failed upgrade included.
//! - [`counters`] is the process-wide set of plain atomics the host reads into its metrics: the
//!   library has no metrics dependency. They count joins by result, closes by contract code,
//!   rejected upgrades by reason, and `resync_required` answers. They are totals of the process,
//!   not of one site: no label may carry a user, room, token or site-specific text.
//!
//! The cap is a resource bound, not an authentication step: it applies before `auth`, so an
//! unauthenticated client cannot hold a slot longer than the 10 s auth deadline, but one abusive
//! client can still fill it. Per-address and per-principal limits stay with the server's
//! connection lease and the edge proxy.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The close codes of the contract's table, in the order [`CounterSnapshot::closes`] reports
/// them.
pub const CLOSE_CODES: [u16; 8] = [1000, 1001, 1003, 1008, 1009, 4001, 4008, 4009];

/// The highest cap an operator may set: the server's own limits make a larger one meaningless.
pub const MAX_CONNECTIONS_CEILING: usize = 100_000;

/// What the route is built with besides [`WsConfig`](super::WsConfig): the connection gate.
/// `WsLimits::default()` is unlimited, so `router` behaves exactly as before.
#[derive(Clone, Debug, Default)]
pub struct WsLimits {
    /// The gate every accepted upgrade passes; the host keeps a clone to read the count.
    pub gate: ConnectionGate,
}

impl WsLimits {
    /// Limits with a cap of `max` open sockets (`None` is unlimited).
    #[must_use]
    pub fn with_max_connections(max: Option<usize>) -> Self {
        Self {
            gate: ConnectionGate::new(max),
        }
    }
}

/// An atomic count of open sockets and an optional cap. Clones share the count.
#[derive(Clone, Debug, Default)]
pub struct ConnectionGate {
    open: Arc<AtomicUsize>,
    max: Option<usize>,
}

impl ConnectionGate {
    /// A gate that admits at most `max` sockets (`None` admits any number).
    #[must_use]
    pub fn new(max: Option<usize>) -> Self {
        Self {
            open: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// Takes a slot, or `None` when the cap is reached. The slot is returned when the guard drops.
    #[must_use]
    pub fn try_enter(&self) -> Option<ConnectionGuard> {
        let mut current = self.open.load(Ordering::Acquire);
        loop {
            if self.max.is_some_and(|max| current >= max) {
                return None;
            }
            match self.open.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ConnectionGuard {
                        open: Arc::clone(&self.open),
                    });
                }
                Err(seen) => current = seen,
            }
        }
    }

    /// Sockets open right now (authenticated or not).
    #[must_use]
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    /// The cap, if any.
    #[must_use]
    pub fn max(&self) -> Option<usize> {
        self.max
    }
}

/// One occupied slot of a [`ConnectionGate`]; dropping it frees the slot.
#[derive(Debug)]
pub struct ConnectionGuard {
    open: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The process-wide counters. Read them with [`WsCounters::snapshot`].
#[derive(Debug, Default)]
pub struct WsCounters {
    joins_ok: AtomicU64,
    joins_denied: AtomicU64,
    joins_resync: AtomicU64,
    closes: [AtomicU64; 8],
    rejected_cap: AtomicU64,
    rejected_bad_request: AtomicU64,
    resync_required: AtomicU64,
}

/// A copy of the counters at one instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    /// Joins that succeeded (`joined`, `replayed`).
    pub joins_ok: u64,
    /// Joins the authorizer or the room name refused.
    pub joins_denied: u64,
    /// Joins answered `resync_required`.
    pub joins_resync: u64,
    /// Closes sent, by [`CLOSE_CODES`] in that order.
    pub closes: [u64; 8],
    /// Upgrades refused with 429 at the cap.
    pub rejected_cap: u64,
    /// Upgrades refused with 400 (not a WebSocket, or no `spot.v1`).
    pub rejected_bad_request: u64,
    /// `resync_required` answers (the same events as `joins_resync`).
    pub resync_required: u64,
}

static COUNTERS: WsCounters = WsCounters {
    joins_ok: AtomicU64::new(0),
    joins_denied: AtomicU64::new(0),
    joins_resync: AtomicU64::new(0),
    closes: [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ],
    rejected_cap: AtomicU64::new(0),
    rejected_bad_request: AtomicU64::new(0),
    resync_required: AtomicU64::new(0),
};

/// The process-wide counters.
#[must_use]
pub fn counters() -> &'static WsCounters {
    &COUNTERS
}

impl WsCounters {
    pub(crate) fn join_ok(&self) {
        self.joins_ok.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn join_denied(&self) {
        self.joins_denied.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn join_resync(&self) {
        self.joins_resync.fetch_add(1, Ordering::Relaxed);
        self.resync_required.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn close(&self, code: u16) {
        if let Some(index) = CLOSE_CODES.iter().position(|c| *c == code) {
            self.closes[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn reject_cap(&self) {
        self.rejected_cap.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reject_bad_request(&self) {
        self.rejected_bad_request.fetch_add(1, Ordering::Relaxed);
    }

    /// The counters now.
    #[must_use]
    pub fn snapshot(&self) -> CounterSnapshot {
        let mut closes = [0_u64; 8];
        for (slot, counter) in closes.iter_mut().zip(self.closes.iter()) {
            *slot = counter.load(Ordering::Relaxed);
        }
        CounterSnapshot {
            joins_ok: self.joins_ok.load(Ordering::Relaxed),
            joins_denied: self.joins_denied.load(Ordering::Relaxed),
            joins_resync: self.joins_resync.load(Ordering::Relaxed),
            closes,
            rejected_cap: self.rejected_cap.load(Ordering::Relaxed),
            rejected_bad_request: self.rejected_bad_request.load(Ordering::Relaxed),
            resync_required: self.resync_required.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_admits_up_to_the_cap_and_frees_slots_on_drop() {
        let gate = ConnectionGate::new(Some(2));
        let a = gate.try_enter();
        let b = gate.try_enter();
        assert!(a.is_some() && b.is_some());
        assert!(gate.try_enter().is_none());
        assert_eq!(gate.open(), 2);
        drop(a);
        assert_eq!(gate.open(), 1);
        assert!(gate.try_enter().is_some());
    }

    #[test]
    fn no_cap_means_unlimited_and_a_thousand_cycles_leave_zero() {
        let gate = ConnectionGate::new(None);
        for _ in 0..1000 {
            let guard = gate.try_enter();
            assert!(guard.is_some());
        }
        assert_eq!(gate.open(), 0);
        let held: Vec<_> = (0..50).filter_map(|_| gate.try_enter()).collect();
        assert_eq!(held.len(), 50);
        assert_eq!(gate.open(), 50);
    }

    #[test]
    fn a_default_gate_is_unlimited() {
        assert_eq!(ConnectionGate::default().max(), None);
        assert_eq!(WsLimits::default().gate.max(), None);
    }
}
