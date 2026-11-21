//! The pure parts of the limits: no clock, no I/O; every method takes `now_ms`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// A token bucket: `rate` tokens a second, a burst of `rate`, whole tokens only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenBucket {
    rate: u32,
    /// Tokens times 1000, so refill stays integer.
    milli: u64,
    last_ms: i64,
}

impl TokenBucket {
    /// A full bucket of `rate` tokens, refilling at `rate` a second, created at `now_ms`.
    #[must_use]
    pub fn new(rate: u32, now_ms: i64) -> Self {
        Self {
            rate,
            milli: u64::from(rate) * 1000,
            last_ms: now_ms,
        }
    }

    /// Takes one token if there is one.
    pub fn try_take(&mut self, now_ms: i64) -> bool {
        let elapsed = u64::try_from(now_ms.saturating_sub(self.last_ms)).unwrap_or(0);
        self.last_ms = self.last_ms.max(now_ms);
        let cap = u64::from(self.rate) * 1000;
        self.milli = self
            .milli
            .saturating_add(elapsed.saturating_mul(u64::from(self.rate)))
            .min(cap);
        if self.milli >= 1000 {
            self.milli -= 1000;
            true
        } else {
            false
        }
    }
}

/// The bounds of the hub listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Concurrent requests one node may hold open.
    pub per_node_connections: u32,
    /// Push batches a node may send per second.
    pub push_per_second: u32,
    /// Concurrent requests the whole host admits.
    pub global_connections: u32,
    /// The least time between two audit records for one node, in milliseconds.
    pub audit_interval_ms: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_node_connections: 2,
            push_per_second: 20,
            global_connections: 2_000,
            audit_interval_ms: 60_000,
        }
    }
}

/// Why a request was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The node already holds its share of connections.
    PerNode,
    /// The host is at its ceiling.
    Global,
    /// The node pushes faster than its rate.
    PushRate,
}

impl Refusal {
    /// A short stable word for counters and audit.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::PerNode => "per_node_limit",
            Self::Global => "global_limit",
            Self::PushRate => "push_rate",
        }
    }
}

#[derive(Debug)]
struct NodeState {
    open: u32,
    bucket: TokenBucket,
}

#[derive(Debug, Default)]
struct State {
    nodes: BTreeMap<String, NodeState>,
    total: u32,
}

/// Admission control: per-node and global connection counts, and the push rate per node.
#[derive(Debug)]
pub struct Admission {
    limits: Limits,
    state: Mutex<State>,
}

/// A held connection slot; dropping it frees the slot.
#[derive(Debug)]
pub struct Permit {
    admission: Arc<Admission>,
    node: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.admission.lock();
        state.total = state.total.saturating_sub(1);
        if let Some(node) = state.nodes.get_mut(&self.node) {
            node.open = node.open.saturating_sub(1);
        }
    }
}

impl Admission {
    /// An admission control with `limits`.
    #[must_use]
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The limits.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Admits one request of `node`, or says why not.
    ///
    /// # Errors
    /// [`Refusal::Global`] first (the host ceiling), then [`Refusal::PerNode`].
    pub fn enter(self: &Arc<Self>, node: &str, now_ms: i64) -> Result<Permit, Refusal> {
        let mut state = self.lock();
        if state.total >= self.limits.global_connections {
            return Err(Refusal::Global);
        }
        let rate = self.limits.push_per_second;
        let entry = state
            .nodes
            .entry(node.to_owned())
            .or_insert_with(|| NodeState {
                open: 0,
                bucket: TokenBucket::new(rate, now_ms),
            });
        if entry.open >= self.limits.per_node_connections {
            return Err(Refusal::PerNode);
        }
        entry.open += 1;
        state.total += 1;
        Ok(Permit {
            admission: Arc::clone(self),
            node: node.to_owned(),
        })
    }

    /// Takes one push token for `node`.
    ///
    /// # Errors
    /// [`Refusal::PushRate`] when the node's bucket is empty.
    pub fn allow_push(&self, node: &str, now_ms: i64) -> Result<(), Refusal> {
        let rate = self.limits.push_per_second;
        let mut state = self.lock();
        let entry = state
            .nodes
            .entry(node.to_owned())
            .or_insert_with(|| NodeState {
                open: 0,
                bucket: TokenBucket::new(rate, now_ms),
            });
        if entry.bucket.try_take(now_ms) {
            Ok(())
        } else {
            Err(Refusal::PushRate)
        }
    }

    /// Requests in flight now.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.lock().total
    }
}

/// At most one audit record per key per interval, so a refusal flood is one line a minute.
#[derive(Debug)]
pub struct AuditThrottle {
    interval_ms: i64,
    last: Mutex<BTreeMap<String, i64>>,
}

impl AuditThrottle {
    /// A throttle of one record per `interval_ms` per key.
    #[must_use]
    pub fn new(interval_ms: i64) -> Self {
        Self {
            interval_ms,
            last: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether a record for `key` should be written at `now_ms` (and remembers that it is).
    pub fn should_audit(&self, key: &str, now_ms: i64) -> bool {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match last.get(key) {
            Some(at) if now_ms.saturating_sub(*at) < self.interval_ms => false,
            _ => {
                if last.len() >= 10_000 {
                    // A hostile key space cannot grow this without bound: forget the oldest.
                    if let Some(oldest) = last
                        .iter()
                        .min_by_key(|(_, at)| **at)
                        .map(|(key, _)| key.clone())
                    {
                        last.remove(&oldest);
                    }
                }
                last.insert(key.to_owned(), now_ms);
                true
            }
        }
    }
}
