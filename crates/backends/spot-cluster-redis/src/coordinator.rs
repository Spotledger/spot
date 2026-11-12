//! [`RedisCoordinator`]: the [`Coordinator`] over Redis/Valkey, one atomic Lua call per operation.
//!
//! # What is stored
//!
//! One hash per lease name holds `holder`, `token`, `id` and `expires_us`; a separate counter
//! holds the token high-water mark so a token is never reused, even across a release or an
//! expiry. Counters are decimal strings (a `u64` does not fit Redis' signed integer, so the Lua
//! does exact decimal arithmetic), and a rate limit is one GCRA bucket per key. Every mutation is
//! one [`Script`] invocation, so it is atomic however many nodes race.
//!
//! # Time is an argument, and the lock is not the fence
//!
//! The scripts read no server clock: this backend passes the instant from the injected
//! [`Clock`](spot_core::Clock), so the contract suite drives a real server with a `ManualClock`.
//! In production the nodes must share a clock source (NTP): a lease's `expires_us` is compared
//! against that shared instant, and a node whose clock is behind could otherwise see a lease as
//! still live after another node took it over.
//!
//! Even with a shared clock, **the lock is not claimed to be safe against a pause or a clock
//! jump beyond the fencing token**. A holder that is stopped (a long GC pause, a suspended VM)
//! past its ttl loses the lease without knowing it, and two holders may briefly believe they hold
//! it. That is why a store layer that writes under the lock checks the fencing token inside its
//! own transaction ([`Coordinator::check_fence`]): the token is what refuses a stale writer, and
//! the lock only arranges for one live writer at a time. This backend cannot make that check and
//! a write one atomic step — Redis has no transaction with the storage layer — so a caller that
//! needs it does the check where the write happens.
//!
//! # Failure
//!
//! A Redis error becomes `coordinator.unavailable`; the caller falls back to the database
//! coordinator or fails closed, exactly as the trait specifies.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use redis::Script;
use spot_coordinator::coordinator::{BoxFuture, Coordinator};
use spot_coordinator::error::COUNTER_OVERFLOW;
use spot_coordinator::{
    Admit, Error, FencingToken, Lease, MAX_TTL, RateLimit, Result as CoordResult, validate_name,
};
use spot_core::Clock;
use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::connection::RedisConnection;
use crate::scripts;

/// The coordinator's unavailable code, in the catalogued `coordinator.*` family.
pub const UNAVAILABLE: &str = "coordinator.unavailable";

/// How long `acquire` and `current` wait for this instance's own abandoned acquires to finish
/// their compensating release before giving up with `coordinator.unavailable`.
pub const ABANDONED_WAIT: Duration = Duration::from_secs(5);

/// The [`Coordinator`] over one Redis/Valkey server.
#[derive(Clone)]
pub struct RedisCoordinator {
    connection: RedisConnection,
    clock: Arc<dyn Clock>,
    waker: Arc<Notify>,
    abandoned: Arc<Abandoned>,
    abandoned_wait: Duration,
}

/// The acquires this instance (and its clones) abandoned that have not yet finished: their
/// script may still land and take a lease, and the compensating release has not yet run.
///
/// While the count is above zero, the name may look free and then be taken, so `acquire` and
/// `current` wait for zero first (bounded by [`RedisCoordinator::with_abandoned_wait`]). Only this
/// instance's own attempts are waited for; a lease taken by another node is unaffected.
#[derive(Debug, Default)]
struct Abandoned {
    pending: AtomicUsize,
    idle: Notify,
}

impl Abandoned {
    fn begin(&self) {
        self.pending.fetch_add(1, Ordering::SeqCst);
    }

    fn finish(&self) {
        if self.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }

    /// Waits until no abandoned acquire is outstanding, or `limit` passes (`false`).
    async fn drained(&self, limit: Duration) -> bool {
        let wait = async {
            loop {
                // Register before checking, so a `finish` in between is not missed.
                let notified = self.idle.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.pending.load(Ordering::SeqCst) == 0 {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(limit, wait).await.is_ok()
    }
}

/// Where one spawned acquire stands, shared by the task and the caller's future.
enum Slot {
    /// The script is in flight and the caller still waits.
    Running,
    /// The caller dropped its future first: the task owns the compensating release.
    Abandoned,
    /// The script answered and the caller has not yet taken the answer.
    Done(CoordResult<Lease>),
    /// The caller has the answer (or the attempt needs nothing more).
    Taken,
}

fn locked(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Held by the caller's future: dropping it before the answer is taken abandons the attempt and
/// makes sure a lease the attempt takes is released.
struct Attempt {
    slot: Arc<Mutex<Slot>>,
    owner: RedisCoordinator,
}

impl Attempt {
    fn take(&self) -> Option<CoordResult<Lease>> {
        match std::mem::replace(&mut *locked(&self.slot), Slot::Taken) {
            Slot::Done(result) => Some(result),
            _ => None,
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        let mut slot = locked(&self.slot);
        match std::mem::replace(&mut *slot, Slot::Taken) {
            // The task is still running: it releases when it finishes. Counted under the slot
            // lock, so no caller can see "no abandoned acquire" between here and the release.
            Slot::Running => {
                self.owner.abandoned.begin();
                *slot = Slot::Abandoned;
            }
            // The answer arrived but nobody took it: release the lease it carries.
            Slot::Done(Ok(lease)) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    self.owner.abandoned.begin();
                    let owner = self.owner.clone();
                    runtime.spawn(async move {
                        let _ = owner.release_once(&lease).await;
                        owner.abandoned.finish();
                    });
                }
            }
            other => *slot = other,
        }
    }
}

impl std::fmt::Debug for RedisCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisCoordinator")
            .field("keyspace", &self.connection.keyspace().prefix())
            .finish_non_exhaustive()
    }
}

impl RedisCoordinator {
    /// A coordinator over `connection`, reading time from `clock`.
    #[must_use]
    pub fn new(connection: RedisConnection, clock: Arc<dyn Clock>) -> Self {
        Self {
            connection,
            clock,
            waker: Arc::new(Notify::new()),
            abandoned: Arc::new(Abandoned::default()),
            abandoned_wait: ABANDONED_WAIT,
        }
    }

    /// Changes how long `acquire` and `current` wait for this instance's abandoned acquires
    /// (default [`ABANDONED_WAIT`]); past it they fail closed with `coordinator.unavailable`.
    #[must_use]
    pub fn with_abandoned_wait(mut self, wait: Duration) -> Self {
        self.abandoned_wait = wait;
        self
    }

    /// Waits for every abandoned acquire of this instance to finish its release.
    async fn settle(&self) -> CoordResult<()> {
        if self.abandoned.drained(self.abandoned_wait).await {
            Ok(())
        } else {
            Err(Error::new(
                UNAVAILABLE,
                "an abandoned acquire has not finished releasing",
            ))
        }
    }

    /// The key prefix in use.
    #[must_use]
    pub fn keyspace(&self) -> &crate::keys::KeySpace {
        self.connection.keyspace()
    }

    /// The instant now, in whole microseconds since the Unix epoch, from the injected clock.
    fn now_us(&self) -> CoordResult<i64> {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000)
            .map_err(|_| Error::invalid_argument("the clock is outside the microsecond range"))
    }

    /// The monotonic instant, in whole microseconds, from the injected clock.
    fn mono_us(&self) -> CoordResult<i64> {
        i64::try_from(self.clock.monotonic().as_micros())
            .map_err(|_| Error::invalid_argument("the monotonic clock is outside the range"))
    }

    /// `expires_at` for a ttl, on the injected wall clock.
    fn expires_at(&self, ttl: Duration) -> CoordResult<OffsetDateTime> {
        let ttl = time::Duration::try_from(ttl)
            .map_err(|_| Error::invalid_argument("the ttl is too large"))?;
        self.clock
            .now_utc()
            .checked_add(ttl)
            .ok_or_else(|| Error::invalid_argument("the ttl runs past the last date"))
    }
}

/// Validates a ttl and returns it in whole microseconds (mirrors the database backend).
fn ttl_micros(ttl: Duration) -> CoordResult<i64> {
    if ttl.is_zero() || ttl > MAX_TTL {
        return Err(Error::invalid_argument(format!(
            "a ttl is more than zero and at most a year, not {ttl:?}"
        )));
    }
    i64::try_from(ttl.as_micros())
        .map_err(|_| Error::invalid_argument("the ttl is too large to express in microseconds"))
}

fn unavailable(what: &str, error: redis::RedisError) -> Error {
    Error::new(UNAVAILABLE, format!("{what}: {error}"))
}

fn lost(lease: &Lease) -> Error {
    Error::lease_lost(
        &lease.name,
        &lease.holder,
        lease.token.0,
        "the lease expired, was released or was taken over",
    )
}

fn parse_u64(text: &str, what: &str) -> CoordResult<u64> {
    text.parse::<u64>()
        .map_err(|_| Error::new(UNAVAILABLE, format!("{what}: {text:?} is not a u64")))
}

fn lease_of(
    name: &str,
    holder: String,
    token: &str,
    id: &str,
    expires_at: OffsetDateTime,
) -> CoordResult<Lease> {
    Ok(Lease {
        name: name.to_owned(),
        holder,
        token: FencingToken(parse_u64(token, "a fencing token")?),
        expires_at,
        id: parse_u64(id, "a lease id")?,
    })
}

/// The parts of the coordinator that can run in a spawned task, so a cancelled `acquire` cannot
/// leak a lease (see [`Coordinator::acquire`]).
impl RedisCoordinator {
    /// Runs the acquire script with pre-computed time. Owned arguments, so it can be spawned.
    async fn acquire_once(
        &self,
        name: &str,
        holder: &str,
        ttl_us: i64,
        now: i64,
        expires_at: OffsetDateTime,
    ) -> CoordResult<Lease> {
        let mut manager = self.connection.manager();
        let reply: Vec<String> = Script::new(&scripts::lease_acquire())
            .key(self.keyspace().lease(name))
            .key(self.keyspace().token(name))
            .arg(holder)
            .arg(ttl_us)
            .arg(now)
            .invoke_async(&mut manager)
            .await
            .map_err(|error| unavailable("acquire", error))?;
        match reply.first().map(String::as_str) {
            Some("ok") => lease_of(
                name,
                reply.get(1).cloned().unwrap_or_default(),
                reply.get(2).map(String::as_str).unwrap_or("0"),
                reply.get(3).map(String::as_str).unwrap_or("0"),
                expires_at,
            ),
            Some("held") => Err(Error::lease_held(
                name,
                holder,
                reply.get(1).map(String::as_str).unwrap_or(""),
                reply
                    .get(2)
                    .and_then(|token| token.parse::<u64>().ok())
                    .unwrap_or(0),
            )),
            _ => Err(Error::new(
                UNAVAILABLE,
                format!("acquire {name}: unexpected reply {reply:?}"),
            )),
        }
    }

    /// Runs the release script; `true` when the lease was still current.
    async fn release_once(&self, lease: &Lease) -> CoordResult<bool> {
        let now = self.now_us()?;
        let mut manager = self.connection.manager();
        let released: i64 = Script::new(&scripts::lease_release())
            .key(self.keyspace().lease(&lease.name))
            .arg(lease.token.0.to_string())
            .arg(lease.id.to_string())
            .arg(now)
            .invoke_async(&mut manager)
            .await
            .map_err(|error| unavailable("release", error))?;
        Ok(released != 0)
    }
}

impl Coordinator for RedisCoordinator {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        Box::pin(async move {
            validate_name(name)?;
            validate_name(holder)?;
            let ttl_us = ttl_micros(ttl)?;
            self.settle().await?;
            let now = self.now_us()?;
            let expires_at = self.expires_at(ttl)?;
            // Cancellation safety: the script runs in a spawned task and the lease it may have
            // taken is released if the caller dropped this future before the answer arrived.
            // Until that release has run, every later `acquire` and `current` of this instance
            // waits (`settle`), so an abandoned attempt can neither be observed as "free, then
            // taken" nor block its own caller's retry.
            let this = self.clone();
            let name = name.to_owned();
            let holder = holder.to_owned();
            let slot = Arc::new(Mutex::new(Slot::Running));
            let attempt = Attempt {
                slot: Arc::clone(&slot),
                owner: self.clone(),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let result = this
                    .acquire_once(&name, &holder, ttl_us, now, expires_at)
                    .await;
                // `Some(result)` when the caller abandoned the attempt: this task releases.
                let leftover = {
                    let mut guard = locked(&slot);
                    match std::mem::replace(&mut *guard, Slot::Taken) {
                        Slot::Running => {
                            *guard = Slot::Done(result);
                            None
                        }
                        Slot::Abandoned => Some(result),
                        other => {
                            *guard = other;
                            None
                        }
                    }
                };
                match leftover {
                    Some(result) => {
                        if let Ok(lease) = result {
                            let _ = this.release_once(&lease).await;
                        }
                        this.abandoned.finish();
                    }
                    None => {
                        let _ = tx.send(());
                    }
                }
            });
            if rx.await.is_err() {
                return Err(Error::new(UNAVAILABLE, "the acquire task did not answer"));
            }
            attempt
                .take()
                .unwrap_or_else(|| Err(Error::new(UNAVAILABLE, "the acquire task did not answer")))
        })
    }

    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        Box::pin(async move {
            let ttl_us = ttl_micros(ttl)?;
            let now = self.now_us()?;
            let expires_at = self.expires_at(ttl)?;
            let mut manager = self.connection.manager();
            let reply: Vec<String> = Script::new(&scripts::lease_renew())
                .key(self.keyspace().lease(&lease.name))
                .arg(lease.token.0.to_string())
                .arg(lease.id.to_string())
                .arg(ttl_us)
                .arg(now)
                .invoke_async(&mut manager)
                .await
                .map_err(|error| unavailable("renew", error))?;
            match reply.first().map(String::as_str) {
                Some("ok") => lease_of(
                    &lease.name,
                    reply.get(1).cloned().unwrap_or_default(),
                    reply.get(2).map(String::as_str).unwrap_or("0"),
                    reply.get(3).map(String::as_str).unwrap_or("0"),
                    expires_at,
                ),
                _ => Err(lost(lease)),
            }
        })
    }

    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        Box::pin(async move {
            if !self.release_once(lease).await? {
                return Err(lost(lease));
            }
            self.waker.notify_waiters();
            Ok(())
        })
    }

    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        Box::pin(async move {
            validate_name(name)?;
            self.settle().await?;
            let now = self.now_us()?;
            let mut manager = self.connection.manager();
            let fields: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(self.keyspace().lease(name))
                .arg("holder")
                .arg("token")
                .arg("id")
                .arg("expires_us")
                .query_async(&mut manager)
                .await
                .map_err(|error| unavailable("current", error))?;
            let holder = fields.first().cloned().flatten();
            let token = fields.get(1).cloned().flatten();
            let id = fields.get(2).cloned().flatten();
            let expires = fields
                .get(3)
                .cloned()
                .flatten()
                .and_then(|text| text.parse::<i64>().ok());
            match (holder, token, id, expires) {
                (Some(holder), Some(token), Some(id), Some(expires)) if expires > now => {
                    let expires_at =
                        OffsetDateTime::UNIX_EPOCH + time::Duration::microseconds(expires);
                    Ok(Some(lease_of(name, holder, &token, &id, expires_at)?))
                }
                _ => Ok(None),
            }
        })
    }

    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        Box::pin(async move {
            validate_name(name)?;
            let now = self.now_us()?;
            let mut manager = self.connection.manager();
            let fields: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(self.keyspace().lease(name))
                .arg("holder")
                .arg("token")
                .arg("expires_us")
                .query_async(&mut manager)
                .await
                .map_err(|error| unavailable("check_fence", error))?;
            let holder = fields.first().cloned().flatten();
            let current = fields.get(1).cloned().flatten();
            let expires = fields
                .get(2)
                .cloned()
                .flatten()
                .and_then(|text| text.parse::<i64>().ok());
            let live = expires.is_some_and(|expires| expires > now);
            match (holder, current, live) {
                (Some(_), Some(current), true) if current == token.0.to_string() => Ok(()),
                (Some(holder), Some(current), true) => Err(Error::lease_lost(
                    name,
                    &holder,
                    token.0,
                    &format!("token {current} holds it now"),
                )),
                _ => Err(Error::lease_lost(
                    name,
                    "",
                    token.0,
                    "the lease expired or was released",
                )),
            }
        })
    }

    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        Box::pin(async move {
            validate_name(name)?;
            if by == 0 {
                return Err(Error::invalid_argument("a counter advances by at least 1"));
            }
            let mut manager = self.connection.manager();
            let reply: Vec<String> = Script::new(&scripts::counter_next())
                .key(self.keyspace().counter(name))
                .arg(by.to_string())
                .invoke_async(&mut manager)
                .await
                .map_err(|error| unavailable("next", error))?;
            match reply.first().map(String::as_str) {
                Some("ok") => parse_u64(
                    reply.get(1).map(String::as_str).unwrap_or("0"),
                    "a counter value",
                ),
                Some("overflow") => Err(Error::new(
                    COUNTER_OVERFLOW,
                    format!("counter {name}: the reservation would pass the largest value"),
                )),
                _ => Err(Error::new(
                    UNAVAILABLE,
                    format!("next {name}: unexpected reply {reply:?}"),
                )),
            }
        })
    }

    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        Box::pin(async move {
            validate_name(name)?;
            let mut manager = self.connection.manager();
            let value: Option<String> = redis::cmd("GET")
                .arg(self.keyspace().counter(name))
                .query_async(&mut manager)
                .await
                .map_err(|error| unavailable("peek", error))?;
            match value {
                Some(text) => parse_u64(&text, "a counter value"),
                None => Ok(0),
            }
        })
    }

    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        Box::pin(async move {
            validate_name(name)?;
            let mut manager = self.connection.manager();
            let last: String = Script::new(&scripts::counter_set_at_least())
                .key(self.keyspace().counter(name))
                .arg(value.to_string())
                .invoke_async(&mut manager)
                .await
                .map_err(|error| unavailable("set_at_least", error))?;
            parse_u64(&last, "a counter value")
        })
    }

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        Box::pin(async move {
            validate_name(key)?;
            limit.validate()?;
            if cost == 0 || cost > limit.capacity {
                return Err(Error::invalid_argument(format!(
                    "a take costs between 1 and the capacity {}, not {cost}",
                    limit.capacity
                )));
            }
            let unit = limit.per.as_micros();
            if unit == 0 {
                return Err(Error::invalid_argument(
                    "the rate-limit window is shorter than one microsecond",
                ));
            }
            let unit = i64::try_from(unit)
                .map_err(|_| Error::invalid_argument("the rate-limit window is too large"))?;
            let capacity = i64::try_from(limit.capacity)
                .map_err(|_| Error::invalid_argument("the capacity is too large"))?;
            let cost = i64::try_from(cost)
                .map_err(|_| Error::invalid_argument("the cost is too large"))?;
            let now = self.mono_us()?;
            let mut manager = self.connection.manager();
            let reply: Vec<String> = Script::new(&scripts::rate_take())
                .key(self.keyspace().rate(key))
                .arg(cost)
                .arg(unit)
                .arg(capacity)
                .arg(now)
                .invoke_async(&mut manager)
                .await
                .map_err(|error| unavailable("take", error))?;
            match reply.first().map(String::as_str) {
                Some("ok") => Ok(Admit::Allowed {
                    remaining: reply
                        .get(1)
                        .and_then(|text| text.parse::<u64>().ok())
                        .unwrap_or(0),
                }),
                Some("denied") => {
                    let retry_us = reply
                        .get(1)
                        .and_then(|text| text.parse::<i64>().ok())
                        .unwrap_or(unit)
                        .max(1);
                    Ok(Admit::Denied {
                        retry_after: Duration::from_micros(u64::try_from(retry_us).unwrap_or(1)),
                    })
                }
                _ => Err(Error::new(
                    UNAVAILABLE,
                    format!("take {key}: unexpected reply {reply:?}"),
                )),
            }
        })
    }

    fn waker(&self) -> Arc<Notify> {
        Arc::clone(&self.waker)
    }
}
