//! A [`Coordinator`] decorator that counts lease events for `/metrics` (WP-OPS-01.T1).
//!
//! Every method delegates; the decorator only looks at the outcome. It never changes a return
//! value, never adds a wait and never panics. Labels are the lease *kind*
//! ([`spot_telemetry::metrics::lease_kind`]), never the lease name, which contains the site id.

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::error::LEASE_LOST;
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, FencingToken, Lease, RateLimit, Result,
};
use spot_telemetry::metrics::{lease_kind, record_lease_event, set_lease_held};
use tokio::sync::Notify;

/// Wraps a coordinator and records lease events and the held gauge.
pub struct MeteredCoordinator<C: ?Sized = dyn Coordinator> {
    inner: Arc<C>,
}

impl<C: Coordinator + ?Sized> MeteredCoordinator<C> {
    /// Meters `inner`.
    pub fn new(inner: Arc<C>) -> Self {
        Self { inner }
    }
}

/// Records a failure of a renew, release or fence check: `lost` (and the gauge down) for a
/// stale handle, `error` for anything else.
fn record_failure(kind: &'static str, error: &spot_coordinator::Error) {
    if error.code == LEASE_LOST {
        record_lease_event(kind, "lost");
        set_lease_held(kind, false);
    } else {
        record_lease_event(kind, "error");
    }
}

impl<C: Coordinator + ?Sized + 'static> Coordinator for MeteredCoordinator<C> {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Lease>> {
        Box::pin(async move {
            let result = self.inner.acquire(name, holder, ttl).await;
            let kind = lease_kind(name);
            match &result {
                Ok(_) => {
                    record_lease_event(kind, "acquired");
                    set_lease_held(kind, true);
                }
                // Another holder's live lease is the normal answer of a follower.
                Err(e) if e.code == spot_coordinator::error::LEASE_HELD => {}
                Err(_) => record_lease_event(kind, "error"),
            }
            result
        })
    }

    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, Result<Lease>> {
        Box::pin(async move {
            let result = self.inner.renew(lease, ttl).await;
            let kind = lease_kind(&lease.name);
            match &result {
                Ok(_) => {
                    record_lease_event(kind, "renewed");
                    set_lease_held(kind, true);
                }
                Err(e) => record_failure(kind, e),
            }
            result
        })
    }

    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let result = self.inner.release(lease).await;
            let kind = lease_kind(&lease.name);
            match &result {
                Ok(()) => {
                    record_lease_event(kind, "released");
                    set_lease_held(kind, false);
                }
                Err(e) => record_failure(kind, e),
            }
            result
        })
    }

    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Lease>>> {
        self.inner.current(name)
    }

    fn check_fence<'a>(&'a self, name: &'a str, token: FencingToken) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let result = self.inner.check_fence(name, token).await;
            if let Err(e) = &result {
                let kind = lease_kind(name);
                if e.code == LEASE_LOST {
                    record_lease_event(kind, "fence_refused");
                    record_lease_event(kind, "lost");
                    set_lease_held(kind, false);
                } else {
                    record_lease_event(kind, "error");
                }
            }
            result
        })
    }

    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, Result<u64>> {
        self.inner.next(name, by)
    }

    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<u64>> {
        self.inner.peek(name)
    }

    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, Result<u64>> {
        self.inner.set_at_least(name, value)
    }

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, Result<Admit>> {
        self.inner.take(key, limit, cost)
    }

    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }

    fn release_now(&self, lease: &Lease) -> bool {
        let released = self.inner.release_now(lease);
        if released {
            let kind = lease_kind(&lease.name);
            record_lease_event(kind, "released");
            set_lease_held(kind, false);
        }
        released
    }
}
