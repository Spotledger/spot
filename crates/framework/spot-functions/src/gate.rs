//! The rate gate: the one place a declared function limit is enforced (WP-PLT-17.T8).
//!
//! [`FunctionRegistry::dispatch`](crate::FunctionRegistry::dispatch) asks a [`RateGate`]
//! whether one more call of a function by a caller fits the function's declared limit. The
//! default gate, [`LocalGate`], is the in-process token bucket the crate always had
//! ([`RateLimiter`]): `limit = { permits = 10, per = "1m" }` then means ten calls a minute on
//! this node. A host that runs several nodes installs a gate that shares the buckets (the
//! host's `CoordinatorGate` over the coordinator's `take`), so the same declaration means ten
//! calls a minute for the site.
//!
//! A gate is async because a shared gate talks to a coordinator. It is consulted only for a
//! function that declares a limit, once per call, in the place the local limiter used to be
//! checked. The crate does not catch a panicking gate (no `catch_unwind`): a panic in a gate
//! is a defect of the gate and reaches the caller's task as any other panic would.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use spot_core::clock::Clock;

use crate::limit::{LimitError, RateLimiter};

/// A boxed, `Send` future: the return type of [`RateGate::admit`], so the trait stays
/// object-safe behind `Arc<dyn RateGate>`.
pub type GateFuture<'a> = Pin<Box<dyn Future<Output = Result<(), LimitError>> + Send + 'a>>;

/// What a gate is asked about: the function's display address and the caller's bucket label
/// ([`Caller::bucket`](crate::Caller::bucket): `user:<id>`, `system:<id>` or `anonymous`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GateKey {
    /// The function, as `app/module/name`.
    pub function: String,
    /// The caller's bucket label.
    pub caller: String,
}

/// Admits or refuses one call against a declared limit.
pub trait RateGate: Send + Sync {
    /// Takes one permit for `key` under the limit `permits` per `per`. `Err` carries how long
    /// until a permit is available; a refused call does not consume or owe a permit.
    fn admit<'a>(&'a self, key: &'a GateKey, permits: u32, per: Duration) -> GateFuture<'a>;

    /// Drops the buckets this gate keeps in memory that were idle for longer than
    /// `older_than`; returns how many. A gate that keeps none returns 0.
    fn evict_idle(&self, _older_than: Duration) -> usize {
        0
    }

    /// How many buckets this gate keeps in memory (a gate that keeps none reports 0).
    fn bucket_count(&self) -> usize {
        0
    }
}

/// The in-process gate: one token bucket per `(function, caller)` in this process, over the
/// injectable [`Clock`]. Behaves exactly as the registry's limiter did before gates existed.
pub struct LocalGate {
    limiter: RateLimiter,
}

impl LocalGate {
    /// A local gate reading time from `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> LocalGate {
        LocalGate {
            limiter: RateLimiter::new(clock),
        }
    }

    /// Replaces the clock (tests swap in a manual clock).
    pub fn set_clock(&self, clock: Arc<dyn Clock>) {
        self.limiter.set_clock(clock);
    }

    /// The synchronous check, for a caller that already holds the gate by its concrete type
    /// (a distributed gate's fallback).
    pub fn check(&self, key: &GateKey, permits: u32, per: Duration) -> Result<(), LimitError> {
        self.limiter
            .check(&(key.function.clone(), key.caller.clone()), permits, per)
    }
}

impl RateGate for LocalGate {
    fn admit<'a>(&'a self, key: &'a GateKey, permits: u32, per: Duration) -> GateFuture<'a> {
        let verdict = self.check(key, permits, per);
        Box::pin(async move { verdict })
    }

    fn evict_idle(&self, older_than: Duration) -> usize {
        self.limiter.evict_idle(older_than)
    }

    fn bucket_count(&self) -> usize {
        self.limiter.len()
    }
}
