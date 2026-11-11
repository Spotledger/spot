//! [`InProcessCoordinator`]: the single-node backend. One mutex, time from an injected
//! [`Clock`], expiry checked lazily on every call.
//!
//! It is correct for **one node only**. Two nodes would each grant the same lease, so it
//! refuses to start beside a second node ([`InProcessCoordinator::start`]) and, if one appears
//! while running, refuses every later call ([`InProcessCoordinator::check_presence`]).
//!
//! # Rate limits
//!
//! `take` is the generic cell rate algorithm (a token bucket kept as one number): the bucket
//! refills continuously at `capacity` units per `per`, holds at most `capacity`, and a take of
//! `cost` units is admitted only when that many units are present. The state per key is one
//! integer, the *theoretical arrival time*, scaled by `capacity` so every step is exact
//! integer arithmetic (no rounding drift, no floats). Time comes from `Clock::monotonic`, so a
//! wall-clock jump cannot refill a bucket. Changing the limit a key is used with starts that
//! key's bucket afresh.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::coordinator::{BoxFuture, Coordinator};
use crate::error::{COUNTER_OVERFLOW, Error, Result, SECOND_NODE};
use crate::presence::{NodePresence, Registration};
use crate::types::{
    Admit, FencingToken, Lease, LeaseId, MAX_TTL, NodeId, RateLimit, validate_name,
};
use spot_core::Clock;

/// How often a host loop should call [`InProcessCoordinator::check_presence`]: a second node is
/// found within this long of appearing. Nothing in this crate runs a timer.
pub const PRESENCE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The buckets are swept of idle entries once there are this many (then twice as many, ...).
const SWEEP_FIRST_AT: usize = 1024;

/// One live lease record.
#[derive(Debug, Clone)]
struct Record {
    lease: Lease,
    /// When it ends, on the monotonic clock: expiry never depends on the wall clock.
    ends_at: Duration,
}

/// One name's state.
#[derive(Debug)]
struct LeaseState {
    record: Option<Record>,
    /// The last token ever issued for the name; the next acquisition's token is one more.
    last_token: u64,
    /// The last record id issued; never reused, so stale handles always miss.
    last_id: LeaseId,
}

/// One rate-limit key's bucket.
#[derive(Debug)]
struct Bucket {
    limit: RateLimit,
    /// The theoretical arrival time, in nanoseconds scaled by `limit.capacity`.
    tat: u128,
}

#[derive(Debug)]
struct State {
    leases: HashMap<String, LeaseState>,
    counters: HashMap<String, u64>,
    buckets: HashMap<String, Bucket>,
    sweep_at: usize,
}

/// The single-node [`Coordinator`].
pub struct InProcessCoordinator {
    state: Mutex<State>,
    clock: Arc<dyn Clock>,
    waker: Arc<Notify>,
    presence: Arc<dyn NodePresence>,
    node: NodeId,
    /// Kept so the node stays registered for as long as the coordinator lives.
    _registration: Registration,
    /// Set (for good) once a second node was seen: the message names it.
    second_node: Mutex<Option<String>>,
    /// Mirrors `second_node` so the common path is one atomic load.
    poisoned: AtomicBool,
}

impl InProcessCoordinator {
    /// Registers `node` in `presence` and starts, or refuses with `coordinator.second_node`
    /// (naming the other node) when another node is already registered.
    pub async fn start(
        presence: Arc<dyn NodePresence>,
        node: NodeId,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>> {
        let registration = presence.register(&node).await?;
        let coordinator = Self {
            state: Mutex::new(State {
                leases: HashMap::new(),
                counters: HashMap::new(),
                buckets: HashMap::new(),
                sweep_at: SWEEP_FIRST_AT,
            }),
            clock,
            waker: Arc::new(Notify::new()),
            presence,
            node,
            _registration: registration,
            second_node: Mutex::new(None),
            poisoned: AtomicBool::new(false),
        };
        // On error the coordinator is dropped here, which withdraws the registration.
        coordinator.check_presence().await?;
        Ok(Arc::new(coordinator))
    }

    /// Looks for a second node. Hosts call this every [`PRESENCE_CHECK_INTERVAL`]. When one
    /// has appeared, this and every later call on the coordinator fail with
    /// `coordinator.second_node`, for good: the coordinator never goes back to granting.
    pub async fn check_presence(&self) -> Result<()> {
        self.ensure_alone()?;
        let others = self.presence.others(&self.node).await?;
        if others.is_empty() {
            return Ok(());
        }
        let names = others
            .iter()
            .map(NodeId::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let message = format!(
            "node {} runs the in-process coordinator, which serves one node only, and node {names} is running too; use the database coordinator for a cluster",
            self.node
        );
        *self
            .second_node
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.clone());
        self.poisoned.store(true, Ordering::SeqCst);
        Err(Error::new(SECOND_NODE, message))
    }

    fn ensure_alone(&self) -> Result<()> {
        if !self.poisoned.load(Ordering::SeqCst) {
            return Ok(());
        }
        let message = self
            .second_node
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_default();
        Err(Error::new(SECOND_NODE, message))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Plain maps; a panic elsewhere cannot leave them inconsistent.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn expiry(&self, ttl: Duration) -> Result<(Duration, OffsetDateTime)> {
        if ttl.is_zero() || ttl > MAX_TTL {
            return Err(Error::invalid_argument(format!(
                "a ttl is more than zero and at most a year, not {ttl:?}"
            )));
        }
        let ends_at = self.clock.monotonic() + ttl;
        let expires_at = time::Duration::try_from(ttl)
            .ok()
            .and_then(|ttl| self.clock.now_utc().checked_add(ttl))
            .ok_or_else(|| Error::invalid_argument("the ttl runs past the last date"))?;
        Ok((ends_at, expires_at))
    }

    /// The state of `name` with an expired record already dropped.
    fn live<'s>(&self, state: &'s mut State, name: &str) -> Option<&'s mut LeaseState> {
        let now = self.clock.monotonic();
        let entry = state.leases.get_mut(name)?;
        if entry.record.as_ref().is_some_and(|r| r.ends_at <= now) {
            entry.record = None;
        }
        Some(entry)
    }

    fn do_acquire(&self, name: &str, holder: &str, ttl: Duration) -> Result<Lease> {
        self.ensure_alone()?;
        validate_name(name)?;
        validate_name(holder)?;
        let (ends_at, expires_at) = self.expiry(ttl)?;
        let mut guard = self.lock();
        self.live(&mut guard, name);
        let entry = guard
            .leases
            .entry(name.to_owned())
            .or_insert_with(|| LeaseState {
                record: None,
                last_token: 0,
                last_id: 0,
            });
        if let Some(record) = &entry.record {
            return Err(Error::lease_held(
                name,
                holder,
                &record.lease.holder,
                record.lease.token.0,
            ));
        }
        entry.last_token += 1;
        entry.last_id += 1;
        let lease = Lease {
            name: name.to_owned(),
            holder: holder.to_owned(),
            token: FencingToken(entry.last_token),
            expires_at,
            id: entry.last_id,
        };
        entry.record = Some(Record {
            lease: lease.clone(),
            ends_at,
        });
        Ok(lease)
    }

    /// Why `lease` is not the current record.
    fn stale(lease: &Lease, current: Option<&Record>) -> Error {
        let why = match current {
            Some(record) => format!("token {} holds it now", record.lease.token.0),
            None => "the lease expired or was released".to_owned(),
        };
        Error::lease_lost(&lease.name, &lease.holder, lease.token.0, &why)
    }

    fn is_current(lease: &Lease, current: Option<&Record>) -> bool {
        current.is_some_and(|r| r.lease.id == lease.id && r.lease.token == lease.token)
    }

    fn do_renew(&self, lease: &Lease, ttl: Duration) -> Result<Lease> {
        self.ensure_alone()?;
        let (ends_at, expires_at) = self.expiry(ttl)?;
        let mut guard = self.lock();
        let entry = self.live(&mut guard, &lease.name);
        let current = entry.as_ref().and_then(|e| e.record.as_ref());
        if !Self::is_current(lease, current) {
            return Err(Self::stale(lease, current));
        }
        match entry.and_then(|e| e.record.as_mut()) {
            Some(record) => {
                record.ends_at = ends_at;
                record.lease.expires_at = expires_at;
                Ok(record.lease.clone())
            }
            None => Err(Self::stale(lease, None)),
        }
    }

    fn do_release(&self, lease: &Lease) -> Result<()> {
        self.ensure_alone()?;
        let mut guard = self.lock();
        let entry = self.live(&mut guard, &lease.name);
        let current = entry.as_ref().and_then(|e| e.record.as_ref());
        if !Self::is_current(lease, current) {
            return Err(Self::stale(lease, current));
        }
        if let Some(entry) = entry {
            entry.record = None;
        }
        drop(guard);
        self.waker.notify_waiters();
        Ok(())
    }

    fn do_current(&self, name: &str) -> Result<Option<Lease>> {
        self.ensure_alone()?;
        validate_name(name)?;
        let mut guard = self.lock();
        Ok(self
            .live(&mut guard, name)
            .and_then(|e| e.record.as_ref())
            .map(|r| r.lease.clone()))
    }

    fn do_check_fence(&self, name: &str, token: FencingToken) -> Result<()> {
        self.ensure_alone()?;
        validate_name(name)?;
        let mut guard = self.lock();
        match self.live(&mut guard, name).and_then(|e| e.record.as_ref()) {
            Some(record) if record.lease.token == token => Ok(()),
            Some(record) => Err(Error::lease_lost(
                name,
                &record.lease.holder,
                token.0,
                &format!("token {} holds it now", record.lease.token.0),
            )),
            None => Err(Error::lease_lost(
                name,
                "",
                token.0,
                "the lease expired or was released",
            )),
        }
    }

    fn do_next(&self, name: &str, by: u64) -> Result<u64> {
        self.ensure_alone()?;
        validate_name(name)?;
        if by == 0 {
            return Err(Error::invalid_argument("a counter advances by at least 1"));
        }
        let mut guard = self.lock();
        let last = guard.counters.entry(name.to_owned()).or_insert(0);
        let end = last.checked_add(by).ok_or_else(|| {
            Error::new(
                COUNTER_OVERFLOW,
                format!("counter {name}: {last} + {by} passes the largest value"),
            )
        })?;
        let first = *last + 1;
        *last = end;
        Ok(first)
    }

    fn do_peek(&self, name: &str) -> Result<u64> {
        self.ensure_alone()?;
        validate_name(name)?;
        Ok(self.lock().counters.get(name).copied().unwrap_or(0))
    }

    fn do_set_at_least(&self, name: &str, value: u64) -> Result<u64> {
        self.ensure_alone()?;
        validate_name(name)?;
        let mut guard = self.lock();
        let last = guard.counters.entry(name.to_owned()).or_insert(0);
        *last = (*last).max(value);
        Ok(*last)
    }

    fn do_take(&self, key: &str, limit: RateLimit, cost: u64) -> Result<Admit> {
        self.ensure_alone()?;
        validate_name(key)?;
        limit.validate()?;
        if cost == 0 || cost > limit.capacity {
            return Err(Error::invalid_argument(format!(
                "a take costs between 1 and the capacity {}, not {cost}",
                limit.capacity
            )));
        }
        let capacity = u128::from(limit.capacity);
        // One unit is `unit` scaled nanoseconds; a full bucket is `burst` of them.
        let unit = limit.per.as_nanos();
        let burst = capacity * unit;
        let now = self.clock.monotonic().as_nanos() * capacity;
        let mut guard = self.lock();
        if guard.buckets.len() >= guard.sweep_at {
            // A bucket whose arrival time has passed is full again: forgetting it changes nothing.
            guard.buckets.retain(|_, b| {
                b.tat > self.clock.monotonic().as_nanos() * u128::from(b.limit.capacity)
            });
            guard.sweep_at = (guard.buckets.len() * 2).max(SWEEP_FIRST_AT);
        }
        let tat = match guard.buckets.get(key) {
            Some(bucket) if bucket.limit == limit => bucket.tat,
            _ => 0,
        };
        let level = tat.max(now) + u128::from(cost) * unit - now;
        if level > burst {
            let retry_nanos = (level - burst).div_ceil(capacity);
            return Ok(Admit::Denied {
                retry_after: Duration::from_nanos(u64::try_from(retry_nanos).unwrap_or(u64::MAX)),
            });
        }
        guard.buckets.insert(
            key.to_owned(),
            Bucket {
                limit,
                tat: now + level,
            },
        );
        let remaining = u64::try_from((burst - level) / unit).unwrap_or(u64::MAX);
        Ok(Admit::Allowed { remaining })
    }
}

impl std::fmt::Debug for InProcessCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessCoordinator")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

impl Coordinator for InProcessCoordinator {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Lease>> {
        Box::pin(async move { self.do_acquire(name, holder, ttl) })
    }

    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, Result<Lease>> {
        Box::pin(async move { self.do_renew(lease, ttl) })
    }

    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.do_release(lease) })
    }

    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Lease>>> {
        Box::pin(async move { self.do_current(name) })
    }

    fn check_fence<'a>(&'a self, name: &'a str, token: FencingToken) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.do_check_fence(name, token) })
    }

    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.do_next(name, by) })
    }

    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.do_peek(name) })
    }

    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.do_set_at_least(name, value) })
    }

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, Result<Admit>> {
        Box::pin(async move { self.do_take(key, limit, cost) })
    }

    fn waker(&self) -> Arc<Notify> {
        Arc::clone(&self.waker)
    }

    fn release_now(&self, lease: &Lease) -> bool {
        // A stale lease is already gone; either way nothing is left to do.
        let _ = self.do_release(lease);
        true
    }
}
