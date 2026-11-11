//! The [`Coordinator`] trait, the RAII [`LockGuard`] and the [`LockExt`] that builds it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::error::{Error, LEASE_HELD, LOCK_TIMEOUT, Result};
use crate::types::{Admit, FencingToken, Lease, RateLimit};

/// A boxed future, so [`Coordinator`] is object-safe without a macro crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long a lock lives when the caller does not say: a leaked guard or a crashed node blocks
/// nobody for longer than this.
pub const DEFAULT_LOCK_TTL: Duration = Duration::from_secs(30);

/// How often a lock waiter re-tries on its own, for releases it was not told about (another
/// node's, or an expiry): the wake-up from [`Coordinator::waker`] only covers local releases.
pub const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Everything the platform needs from a coordination store: the operations that may not lose
/// data, in contrast to the cache.
///
/// All operations are cancellation safe: dropping a returned future at any `.await` leaves no
/// lease held, no counter moved and no tokens taken that the caller was not told about.
///
/// *Not* here, by decision: a cluster-wide circuit breaker (ADR-072).
pub trait Coordinator: Send + Sync {
    /// Acquires the named lease for `holder` for `ttl`, or fails with
    /// `coordinator.lease_held` while another lease for the name is live. The token is strictly
    /// larger than every token ever issued for the name.
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Lease>>;

    /// Extends the lease to `ttl` from now, when `lease` is still the current record; a stale
    /// handle fails with `coordinator.lease_lost`. The token does not change.
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, Result<Lease>>;

    /// Gives the lease up, when `lease` is still the current record; a stale handle fails with
    /// `coordinator.lease_lost` and leaves the current holder alone.
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, Result<()>>;

    /// The live lease for `name`, if any.
    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Lease>>>;

    /// Succeeds only for the token of the live lease for `name`; a stale writer's token fails
    /// with `coordinator.lease_lost`. A store layer calls this inside its own transaction so
    /// the check and the write cannot be separated.
    fn check_fence<'a>(&'a self, name: &'a str, token: FencingToken) -> BoxFuture<'a, Result<()>>;

    /// Takes `by` consecutive values from the named counter and returns the first. Atomic,
    /// monotonic, cluster-wide; the first value ever issued is 1. **Not gapless across a
    /// crash**: a caller that dies between `next` and using the values loses them. A gapless
    /// number series is the storage layer's `Sequences`, not this.
    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, Result<u64>>;

    /// The last value issued by the counter (0 when it has never been used). Administration.
    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<u64>>;

    /// Raises the counter so the last value issued is at least `value`; never lowers it.
    /// Returns the counter's last value afterwards. Administration (import, restore).
    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, Result<u64>>;

    /// Takes `cost` units from the rate limit named `key`. Never admits more than `limit`
    /// however many callers race; a denial takes nothing and says how long to wait.
    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, Result<Admit>>;

    /// Woken (`notify_waiters`) whenever this backend releases a lease, so a lock waiter does
    /// not have to wait for its next poll. Releases by other nodes are found by polling.
    fn waker(&self) -> Arc<Notify>;

    /// Releases `lease` immediately without awaiting, if the backend can (an in-process one
    /// can). Returns whether it did. [`LockGuard`]'s drop uses this first and spawns an
    /// `release` only when it returns `false`.
    fn release_now(&self, lease: &Lease) -> bool {
        let _ = lease;
        false
    }
}

/// A held lock. Releases the lease when dropped, best effort; the lease also **always** expires
/// by its ttl, so a leaked guard (`mem::forget`) or a crashed node blocks nobody longer than
/// that.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct LockGuard {
    coordinator: Arc<dyn Coordinator>,
    lease: Option<Lease>,
}

impl LockGuard {
    /// The lease behind the lock: its fencing token fences writes made under the lock.
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    /// Releases the lock now and reports whether the coordinator still saw it as ours
    /// (`coordinator.lease_lost` when it had already expired).
    pub async fn unlock(mut self) -> Result<()> {
        match self.lease.take() {
            Some(lease) => self.coordinator.release(&lease).await,
            None => Ok(()),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if self.coordinator.release_now(&lease) {
            return;
        }
        // No runtime (or one shutting down): the ttl releases it.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let coordinator = Arc::clone(&self.coordinator);
            handle.spawn(async move {
                let _ = coordinator.release(&lease).await;
            });
        }
    }
}

impl std::fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuard")
            .field("lease", &self.lease)
            .finish()
    }
}

/// Locks on a shared coordinator: a lock is a lease with a short ttl and a guard.
pub trait LockExt {
    /// Waits up to `wait` for the lock `name`, holding it for [`DEFAULT_LOCK_TTL`]. Fails with
    /// `coordinator.lock_timeout` after `wait`; `Duration::ZERO` tries exactly once.
    fn lock<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        wait: Duration,
    ) -> BoxFuture<'a, Result<LockGuard>>;

    /// [`LockExt::lock`] with the lock's ttl chosen by the caller.
    fn lock_with_ttl<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        wait: Duration,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<LockGuard>>;
}

impl LockExt for Arc<dyn Coordinator> {
    fn lock<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        wait: Duration,
    ) -> BoxFuture<'a, Result<LockGuard>> {
        self.lock_with_ttl(name, holder, wait, DEFAULT_LOCK_TTL)
    }

    fn lock_with_ttl<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        wait: Duration,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<LockGuard>> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + wait;
            let waker = self.waker();
            loop {
                // Register for a wake-up *before* trying, so a release between the failed
                // attempt and the wait is not missed.
                let notified = waker.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                match self.acquire(name, holder, ttl).await {
                    // No await between getting the lease and returning the guard: dropping
                    // this future can never leave a lease nobody owns.
                    Ok(lease) => {
                        return Ok(LockGuard {
                            coordinator: Arc::clone(self),
                            lease: Some(lease),
                        });
                    }
                    Err(error) if error.code == LEASE_HELD => {}
                    Err(error) => return Err(error),
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(Error::new(
                        LOCK_TIMEOUT,
                        format!("lock {name}: {holder} waited {wait:?} and did not get it"),
                    ));
                }
                let pause = remaining.min(LOCK_POLL_INTERVAL);
                // Woken by a release, or by the poll interval: either way, try again.
                let _ = tokio::time::timeout(pause, notified).await;
            }
        })
    }
}
