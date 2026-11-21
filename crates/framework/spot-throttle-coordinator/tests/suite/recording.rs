//! Wrappers that see (and can break) every `take` a throttle makes.
#![allow(missing_docs, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use spot_coordinator::error::INVALID_ARGUMENT;
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, Error, FencingToken, Lease, RateLimit, Result,
};

/// Delegates everything; remembers the key of every `take`.
pub struct Recording {
    inner: Arc<dyn Coordinator>,
    keys: Mutex<Vec<String>>,
}

impl Recording {
    pub fn new(inner: Arc<dyn Coordinator>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            keys: Mutex::new(Vec::new()),
        })
    }
}

pub trait Handle {
    fn handle(&self) -> Arc<dyn Coordinator>;
}

impl Handle for Arc<Recording> {
    fn handle(&self) -> Arc<dyn Coordinator> {
        Arc::clone(self) as Arc<dyn Coordinator>
    }
}

impl Recording {
    pub fn keys(self: &Arc<Self>) -> Vec<String> {
        self.keys.lock().unwrap().clone()
    }

    /// Keys with the digest replaced by `<digest>` and its length: the shape a key reveals.
    pub fn shapes(self: &Arc<Self>) -> Vec<String> {
        self.keys()
            .iter()
            .map(|key| {
                let (head, digest) = key.rsplit_once(':').unwrap();
                format!("{head}:<{}>", digest.len())
            })
            .collect()
    }
}

macro_rules! delegate {
    ($inner:ident) => {
        fn acquire<'a>(
            &'a self,
            n: &'a str,
            h: &'a str,
            t: Duration,
        ) -> BoxFuture<'a, Result<Lease>> {
            self.$inner.acquire(n, h, t)
        }
        fn renew<'a>(&'a self, l: &'a Lease, t: Duration) -> BoxFuture<'a, Result<Lease>> {
            self.$inner.renew(l, t)
        }
        fn release<'a>(&'a self, l: &'a Lease) -> BoxFuture<'a, Result<()>> {
            self.$inner.release(l)
        }
        fn current<'a>(&'a self, n: &'a str) -> BoxFuture<'a, Result<Option<Lease>>> {
            self.$inner.current(n)
        }
        fn check_fence<'a>(&'a self, n: &'a str, t: FencingToken) -> BoxFuture<'a, Result<()>> {
            self.$inner.check_fence(n, t)
        }
        fn next<'a>(&'a self, n: &'a str, by: u64) -> BoxFuture<'a, Result<u64>> {
            self.$inner.next(n, by)
        }
        fn peek<'a>(&'a self, n: &'a str) -> BoxFuture<'a, Result<u64>> {
            self.$inner.peek(n)
        }
        fn set_at_least<'a>(&'a self, n: &'a str, v: u64) -> BoxFuture<'a, Result<u64>> {
            self.$inner.set_at_least(n, v)
        }
        fn waker(&self) -> Arc<Notify> {
            self.$inner.waker()
        }
    };
}

impl Coordinator for Recording {
    delegate!(inner);

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, Result<Admit>> {
        self.keys.lock().unwrap().push(key.to_owned());
        self.inner.take(key, limit, cost)
    }
}

/// Delegates, but the `fail_on`-th `take` (1-based) and every later one fails.
pub struct Failing {
    inner: Arc<dyn Coordinator>,
    fail_on: u64,
    takes: AtomicU64,
}

impl Failing {
    pub fn new(inner: Arc<dyn Coordinator>, fail_on: u64) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_on,
            takes: AtomicU64::new(0),
        })
    }
}

impl Handle for Arc<Failing> {
    fn handle(&self) -> Arc<dyn Coordinator> {
        Arc::clone(self) as Arc<dyn Coordinator>
    }
}

impl Coordinator for Failing {
    delegate!(inner);

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, Result<Admit>> {
        let n = self.takes.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= self.fail_on {
            return Box::pin(async { Err(Error::new(INVALID_ARGUMENT, "injected failure")) });
        }
        self.inner.take(key, limit, cost)
    }
}
