use crate::AddrClass;
use crate::Purpose;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
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
        self.records.lock().unwrap().clear();
    }
}

impl Default for InMemorySink {
    fn default() -> Self {
        Self::new()
    }
}

impl DenialSink for InMemorySink {
    fn denied(&self, record: DenialRecord) {
        self.records.lock().unwrap().push(record);
    }

    fn records(&self) -> Vec<DenialRecord> {
        self.records.lock().unwrap().clone()
    }
}

/// A rate-limited denial sink that prevents log flooding.
pub struct RateLimitedSink {
    inner: Arc<dyn DenialSink>,
    limits: Mutex<HashMap<String, (u32, std::time::Instant)>>,
    max_per_minute: u32,
}

impl RateLimitedSink {
    /// Create a new rate-limited denial sink.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying denial sink to write to.
    /// * `max_per_minute` - Maximum number of denials to record per minute per key.
    pub fn new(inner: Arc<dyn DenialSink>, max_per_minute: u32) -> Self {
        Self {
            inner,
            limits: Mutex::new(HashMap::new()),
            max_per_minute,
        }
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
}

impl DenialSink for RateLimitedSink {
    fn denied(&self, record: DenialRecord) {
        let key = Self::get_key(&record);
        let mut limits = self.limits.lock().unwrap();
        let now = std::time::Instant::now();

        let (count, last_reset) = limits.entry(key).or_insert((0, now));

        // Reset counter if a minute has passed
        if now.duration_since(*last_reset).as_secs() >= 60 {
            *count = 0;
            *last_reset = now;
        }

        // Only record if under limit
        if *count < self.max_per_minute {
            *count += 1;
            self.inner.denied(record);
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
}
