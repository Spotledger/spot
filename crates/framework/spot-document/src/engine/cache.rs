//! The default site cache for steps (ADR-20): lossy, in memory, with time-to-live.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_app_api::BoxFuture;
use spot_app_api::context::AppCache;
use spot_app_api::prelude::Clock;

/// A cached value and when it expires on the clock's monotonic scale.
type Entry = (Vec<u8>, Option<Duration>);

/// A process-local cache. Entries expire by the engine's clock; any entry may be missing.
pub struct MemoryCache {
    clock: Arc<dyn Clock>,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl MemoryCache {
    /// An empty cache whose time-to-live runs on `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    fn entries(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl AppCache for MemoryCache {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let now = self.clock.monotonic();
            let mut entries = self.entries();
            match entries.get(key) {
                Some((_, Some(expires))) if *expires <= now => {
                    entries.remove(key);
                    None
                }
                Some((value, _)) => Some(value.clone()),
                None => None,
            }
        })
    }

    fn set<'a>(&'a self, key: &'a str, value: Vec<u8>, ttl: Option<Duration>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let expires = ttl.map(|t| self.clock.monotonic().saturating_add(t));
            self.entries().insert(key.to_owned(), (value, expires));
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.entries().remove(key);
        })
    }
}
