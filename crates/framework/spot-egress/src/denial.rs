use crate::AddrClass;
use crate::Purpose;
use spot_core::{Clock, SystemClock};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use time::OffsetDateTime;

/// A record of a denied egress request.
#[derive(Debug, Clone)]
pub struct DenialRecord {
    /// The tenant whose request was denied.
    pub tenant: String,
    /// The purpose of the denied request.
    pub purpose: Purpose,
    /// The actor (user or service) that made the request.
    pub actor: Option<String>,
    /// The scheme (http or https).
    pub scheme: String,
    /// The host that was denied.
    pub host: String,
    /// The port that was denied.
    pub port: u16,
    /// The IP addresses that were resolved for this host.
    pub resolved: Vec<IpAddr>,
    /// The classification of the denied address.
    pub class: AddrClass,
    /// The rule that caused the denial (policy, address, or port).
    pub rule: String,
    /// The time the denial occurred.
    pub at: OffsetDateTime,
}

impl DenialRecord {
    /// Create a privacy-safe summary that doesn't reveal internal addresses.
    pub fn summary(&self) -> String {
        format!(
            "Egress denied for {}/{}: {}/{}:{}",
            self.tenant,
            self.purpose.as_str(),
            self.scheme,
            self.host,
            self.port
        )
    }
}

/// Trait for handling denial records.
pub trait DenialSink: Send + Sync {
    /// Record a denied request.
    fn denied(&self, record: DenialRecord);

    /// Get all recorded denials (for testing).
    fn records(&self) -> Vec<DenialRecord>;
}

/// In-memory denial sink for testing.
pub struct InMemorySink {
    records: Mutex<Vec<DenialRecord>>,
}

impl InMemorySink {
    /// Create a new in-memory denial sink.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }

    /// Clear all recorded denials.
    pub fn clear(&self) {
        self.lock().clear();
    }
}

impl InMemorySink {
    fn lock(&self) -> MutexGuard<'_, Vec<DenialRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for InMemorySink {
    fn default() -> Self {
        Self::new()
    }
}

impl DenialSink for InMemorySink {
    fn denied(&self, record: DenialRecord) {
        self.lock().push(record);
    }

    fn records(&self) -> Vec<DenialRecord> {
        self.lock().clone()
    }
}

/// Default cap on the number of distinct keys a [`RateLimitedSink`] tracks.
pub const DEFAULT_MAX_KEYS: usize = 4096;

/// Length of one rate-limit window.
const WINDOW: Duration = Duration::from_secs(60);

/// One rate-limit window: how many denials were forwarded since it opened.
#[derive(Debug, Clone, Copy)]
struct Window {
    count: u32,
    opened: Duration,
}

impl Window {
    fn expired(&self, now: Duration) -> bool {
        now.saturating_sub(self.opened) >= WINDOW
    }

    /// Opens a new window when the old one is over, then reports whether one more denial fits.
    fn admit(&mut self, now: Duration, max_per_minute: u32) -> bool {
        if self.expired(now) {
            self.count = 0;
            self.opened = now;
        }
        if self.count < max_per_minute {
            self.count += 1;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Default)]
struct Limits {
    keys: HashMap<String, Window>,
    /// The shared bucket for keys that do not fit in `keys`. Kept outside the map so the map
    /// length never exceeds the cap.
    overflow: Option<Window>,
}

/// A rate-limited denial sink that prevents log flooding.
///
/// Memory is bounded: at most `max_keys` distinct tenant/purpose/host/port keys are tracked.
/// When the map is full, keys whose window is over are dropped first; a new key that still
/// does not fit shares one `overflow` bucket, which is rate limited like any other key. The
/// number of denials that were not forwarded is available from [`RateLimitedSink::suppressed`].
pub struct RateLimitedSink {
    inner: Arc<dyn DenialSink>,
    clock: Arc<dyn Clock>,
    limits: Mutex<Limits>,
    max_per_minute: u32,
    max_keys: usize,
    suppressed: AtomicU64,
}

impl RateLimitedSink {
    /// Create a new rate-limited denial sink with the system clock and the default key cap.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying denial sink to write to.
    /// * `max_per_minute` - Maximum number of denials to record per minute per key.
    pub fn new(inner: Arc<dyn DenialSink>, max_per_minute: u32) -> Self {
        Self::with_limits(
            inner,
            max_per_minute,
            DEFAULT_MAX_KEYS,
            Arc::new(SystemClock::new()),
        )
    }

    /// Create a rate-limited sink with an explicit key cap and an injected clock.
    ///
    /// `max_keys` is the largest number of distinct keys tracked at once (a value of zero is
    /// treated as one). Windows are measured on the clock's monotonic reading.
    pub fn with_limits(
        inner: Arc<dyn DenialSink>,
        max_per_minute: u32,
        max_keys: usize,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner,
            clock,
            limits: Mutex::new(Limits::default()),
            max_per_minute,
            max_keys: max_keys.max(1),
            suppressed: AtomicU64::new(0),
        }
    }

    /// The number of denials that were dropped by the rate limit since this sink was created.
    pub fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    /// The number of distinct keys currently tracked (never more than the cap).
    pub fn tracked_keys(&self) -> usize {
        self.lock().keys.len()
    }

    fn lock(&self) -> MutexGuard<'_, Limits> {
        // The guarded data is plain counters; a panic elsewhere cannot leave it inconsistent,
        // so a poisoned lock is recovered rather than propagated.
        self.limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn get_key(record: &DenialRecord) -> String {
        format!(
            "{}/{}/{}/{}",
            record.tenant,
            record.purpose.as_str(),
            record.host,
            record.port
        )
    }

    fn admit(&self, key: String, now: Duration) -> bool {
        let mut limits = self.lock();
        let max_per_minute = self.max_per_minute;
        if let Some(window) = limits.keys.get_mut(&key) {
            return window.admit(now, max_per_minute);
        }
        if limits.keys.len() >= self.max_keys {
            limits.keys.retain(|_, w| !w.expired(now));
        }
        if limits.keys.len() < self.max_keys {
            let mut window = Window {
                count: 0,
                opened: now,
            };
            let ok = window.admit(now, max_per_minute);
            limits.keys.insert(key, window);
            return ok;
        }
        let window = limits.overflow.get_or_insert(Window {
            count: 0,
            opened: now,
        });
        window.admit(now, max_per_minute)
    }
}

impl DenialSink for RateLimitedSink {
    fn denied(&self, record: DenialRecord) {
        let key = Self::get_key(&record);
        let now = self.clock.monotonic();
        // The inner sink is called after the lock is released.
        if self.admit(key, now) {
            self.inner.denied(record);
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn records(&self) -> Vec<DenialRecord> {
        self.inner.records()
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_in_memory_sink() {
        let sink = InMemorySink::new();
        let record = DenialRecord {
            tenant: "test".to_string(),
            purpose: Purpose::Webhook,
            actor: None,
            scheme: "https".to_string(),
            host: "example.com".to_string(),
            port: 443,
            resolved: vec!["127.0.0.1".parse().unwrap()],
            class: AddrClass::Loopback,
            rule: "address".to_string(),
            at: OffsetDateTime::now_utc(),
        };

        sink.denied(record.clone());
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].host, "example.com");
    }

    #[test]
    fn test_denial_record_no_secrets() {
        let record = DenialRecord {
            tenant: "test".to_string(),
            purpose: Purpose::Script,
            actor: Some("user1".to_string()),
            scheme: "https".to_string(),
            host: "internal.example.com".to_string(),
            port: 443,
            resolved: vec!["10.0.0.1".parse().unwrap()],
            class: AddrClass::Private,
            rule: "address".to_string(),
            at: OffsetDateTime::now_utc(),
        };

        let summary = record.summary();
        assert!(summary.contains("denied"));
        assert!(!summary.contains("?token"));
        assert!(!summary.contains("password"));
    }

    #[test]
    fn poisoned_lock_does_not_panic() {
        let inner = Arc::new(InMemorySink::new());
        let sink = Arc::new(RateLimitedSink::new(inner.clone(), 5));
        let s2 = sink.clone();
        let _ = std::thread::spawn(move || {
            let _guard = s2.limits.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(sink.limits.is_poisoned());
        let record = DenialRecord {
            tenant: "t".to_string(),
            purpose: Purpose::Webhook,
            actor: None,
            scheme: "https".to_string(),
            host: "h.example".to_string(),
            port: 443,
            resolved: vec![],
            class: AddrClass::Public,
            rule: "policy".to_string(),
            at: OffsetDateTime::UNIX_EPOCH,
        };
        sink.denied(record);
        assert_eq!(inner.records().len(), 1);
        assert_eq!(sink.tracked_keys(), 1);
    }
}
