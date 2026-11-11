//! In-memory doubles for the host ports. Nothing here is durable or random.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use spot_secrets::WrappedKey;

use crate::ports::{
    AuditPort, Clock, PortError, Rng, StoredKey, SubjectKeyDestroyed, WrappedKeyStore,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A wrapped-key store in memory that can be switched into an outage.
#[derive(Debug, Default)]
pub struct MemoryWrappedKeyStore {
    entries: Mutex<BTreeMap<String, StoredKey>>,
    down: AtomicBool,
}

impl MemoryWrappedKeyStore {
    /// An empty, reachable store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every call fail (`true`) or work again (`false`).
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// The raw entry, bypassing the outage switch.
    #[must_use]
    pub fn peek(&self, entry: &str) -> Option<StoredKey> {
        lock(&self.entries).get(entry).cloned()
    }

    /// Writes an entry directly (to copy one tenant's wrapped key to another's id).
    pub fn poke(&self, entry: &str, value: StoredKey) {
        lock(&self.entries).insert(entry.to_owned(), value);
    }

    /// How many live (not tombstoned) entries there are.
    #[must_use]
    pub fn live_count(&self) -> usize {
        lock(&self.entries)
            .values()
            .filter(|v| matches!(v, StoredKey::Live(_)))
            .count()
    }

    /// Removes an entry outright, tombstone included (a store that lost a row).
    pub fn remove_raw(&self, entry: &str) {
        lock(&self.entries).remove(entry);
    }

    fn check(&self) -> Result<(), PortError> {
        if self.down.load(Ordering::SeqCst) {
            Err(PortError("the store is down".to_owned()))
        } else {
            Ok(())
        }
    }
}

impl WrappedKeyStore for MemoryWrappedKeyStore {
    fn get(&self, entry: &str) -> Result<Option<StoredKey>, PortError> {
        self.check()?;
        Ok(lock(&self.entries).get(entry).cloned())
    }

    fn put_if_absent(&self, entry: &str, wrapped: WrappedKey) -> Result<bool, PortError> {
        self.check()?;
        let mut entries = lock(&self.entries);
        if entries.contains_key(entry) {
            return Ok(false);
        }
        entries.insert(entry.to_owned(), StoredKey::Live(wrapped));
        Ok(true)
    }

    fn destroy(&self, entry: &str) -> Result<bool, PortError> {
        self.check()?;
        let old = lock(&self.entries).insert(entry.to_owned(), StoredKey::Destroyed);
        Ok(matches!(old, Some(StoredKey::Live(_))))
    }
}

/// An audit sink that remembers events and can be made to fail.
#[derive(Debug, Default)]
pub struct MemoryAudit {
    events: Mutex<Vec<SubjectKeyDestroyed>>,
    failing: AtomicBool,
}

impl MemoryAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `record` fail (`true`) or work again.
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// The events recorded so far.
    #[must_use]
    pub fn events(&self) -> Vec<SubjectKeyDestroyed> {
        lock(&self.events).clone()
    }
}

impl AuditPort for MemoryAudit {
    fn record(&self, event: &SubjectKeyDestroyed) -> Result<(), PortError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(PortError("the audit log is down".to_owned()));
        }
        lock(&self.events).push(event.clone());
        Ok(())
    }
}

/// A deterministic byte source, distinct per call: NOT random. Tests only.
#[derive(Debug)]
pub struct CounterRng {
    next: AtomicU64,
    failing: AtomicBool,
}

impl CounterRng {
    /// A source whose first fill is seeded by `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            next: AtomicU64::new(seed),
            failing: AtomicBool::new(false),
        }
    }

    /// Makes `fill` fail (`true`) or work again.
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// How many fills have been served.
    #[must_use]
    pub fn calls(&self, seed: u64) -> u64 {
        self.next.load(Ordering::SeqCst).wrapping_sub(seed)
    }
}

impl Rng for CounterRng {
    fn fill(&self, buf: &mut [u8]) -> Result<(), PortError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(PortError("the random source is down".to_owned()));
        }
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        for (i, byte) in buf.iter_mut().enumerate() {
            let step = u8::try_from(i % 251).unwrap_or(0);
            *byte = n.to_be_bytes()[i % 8] ^ step.wrapping_mul(7).wrapping_add(0x5a);
        }
        Ok(())
    }
}

impl<T: Rng + ?Sized> Rng for std::sync::Arc<T> {
    fn fill(&self, buf: &mut [u8]) -> Result<(), PortError> {
        (**self).fill(buf)
    }
}

/// A clock that always answers one instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_unix_ms(&self) -> i64 {
        self.0
    }
}

/// A clock a test moves by hand; clones share one instant.
#[derive(Debug, Clone, Default)]
pub struct ManualClock(std::sync::Arc<std::sync::atomic::AtomicI64>);

impl ManualClock {
    /// A clock that starts at `start` milliseconds.
    #[must_use]
    pub fn new(start: i64) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
            start,
        )))
    }

    /// Moves the clock forward by `millis`.
    pub fn advance(&self, millis: i64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_unix_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl<T: WrappedKeyStore + ?Sized> WrappedKeyStore for std::sync::Arc<T> {
    fn get(&self, entry: &str) -> Result<Option<StoredKey>, PortError> {
        (**self).get(entry)
    }
    fn put_if_absent(&self, entry: &str, wrapped: WrappedKey) -> Result<bool, PortError> {
        (**self).put_if_absent(entry, wrapped)
    }
    fn destroy(&self, entry: &str) -> Result<bool, PortError> {
        (**self).destroy(entry)
    }
}

impl<T: AuditPort + ?Sized> AuditPort for std::sync::Arc<T> {
    fn record(&self, event: &SubjectKeyDestroyed) -> Result<(), PortError> {
        (**self).record(event)
    }
}
