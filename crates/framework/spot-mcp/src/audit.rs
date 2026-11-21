//! The AI audit chain: every tool call is recorded, allowed or refused.
//!
//! The record carries the tool name, the outcome and the agent identity and tier — never the
//! arguments, which may hold business data or a secret.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_agent_tiers::Tier;
use spot_audit_chain::{AuditChain, AuditRecord, AuditStore, Payload, PayloadValue};

use crate::error::McpError;
use crate::principal::McpPrincipal;

/// The audit kind every MCP call is recorded under.
pub const CALL_KIND: &str = "mcp.call";

/// A sink every tool call is recorded to. Implemented over [`AuditChain`] and, for tests,
/// in memory.
pub trait AuditSink: Send + Sync {
    /// Appends one record.
    ///
    /// # Errors
    ///
    /// A sink failure, mapped to [`McpError::internal`].
    fn record<'a>(&'a self, record: AuditRecord) -> crate::BoxFuture<'a, Result<(), McpError>>;
}

/// An [`AuditSink`] over an [`AuditChain`], the real tamper-evident chain.
pub struct ChainSink<S: AuditStore> {
    chain: AuditChain<S>,
}

impl<S: AuditStore> ChainSink<S> {
    /// Wraps a chain.
    pub fn new(chain: AuditChain<S>) -> Self {
        Self { chain }
    }
}

impl<S: AuditStore + 'static> AuditSink for ChainSink<S> {
    fn record<'a>(&'a self, record: AuditRecord) -> crate::BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            self.chain
                .append(record)
                .await
                .map(|_| ())
                .map_err(|_| McpError::internal("could not record the call"))
        })
    }
}

/// An in-memory audit sink, for tests and for hosts without a chain.
#[derive(Default)]
pub struct MemoryAudit {
    records: Mutex<Vec<AuditRecord>>,
}

impl MemoryAudit {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The records appended so far.
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// How many records were appended.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// Whether no record was appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AuditSink for MemoryAudit {
    fn record<'a>(&'a self, record: AuditRecord) -> crate::BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            if let Ok(mut records) = self.records.lock() {
                records.push(record);
            }
            Ok(())
        })
    }
}

/// The payload key that marks an intent record, written before a call runs.
pub const PHASE_KEY: &str = "phase";
/// The [`PHASE_KEY`] value of an intent record.
pub const PHASE_INTENT: &str = "intent";

/// What the server does when the audit sink refuses a record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuditFailurePolicy {
    /// Every call is refused when its record cannot be written (an intent record is written
    /// first, for reads too, and the result of a read is withheld if its completion record fails).
    FailClosed,
    /// A tool that writes or has side effects is refused when its intent record cannot be
    /// written. A read-only tool proceeds, but the failure is counted, alerted and retried.
    #[default]
    FailClosedForWrites,
    /// No call is refused; every failure is counted, alerted and retried.
    AlertOnly,
}

/// Which record failed to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuditPhase {
    /// The record written before a call runs.
    Intent,
    /// The record written after a call ran.
    Completion,
}

/// A structured alert: an audit write failed. It names the tool and the phase, never the
/// arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditAlert {
    /// The tool whose record could not be written.
    pub tool: String,
    /// Which record failed.
    pub phase: AuditPhase,
    /// The policy in force.
    pub policy: AuditFailurePolicy,
    /// Whether the call was refused because of it.
    pub refused: bool,
    /// When it happened, in microseconds: the caller's clock, never read here.
    pub at_micros: i64,
}

/// Where audit-failure alerts go. The host wires this to its alerting; the default drops them
/// (the counters still count).
pub trait AuditAlerts: Send + Sync {
    /// Raises one alert.
    fn alert(&self, alert: &AuditAlert);
}

/// An alert sink that drops the alert. For tests only: in production an audit outage must not
/// be silent, so [`TelemetryAlerts`] is the default.
#[doc(hidden)]
pub struct NoAlerts;

impl AuditAlerts for NoAlerts {
    fn alert(&self, _alert: &AuditAlert) {}
}

/// The default [`AuditAlerts`]: one structured `tracing` event at error level, target
/// `spot_mcp::audit`, carrying the tool, the phase, the policy and whether the call was refused,
/// never an argument or record content. Identical alerts (same tool and phase) are rate limited
/// to one per [`ALERT_INTERVAL_MICROS`] of the alert's own time; the suppressed ones are counted
/// and reported on the next event that gets through.
#[derive(Default)]
pub struct TelemetryAlerts {
    state: Mutex<HashMap<(String, AuditPhase), AlertState>>,
    suppressed: AtomicU64,
}

#[derive(Clone, Copy)]
struct AlertState {
    last_emitted: i64,
    suppressed_since: u64,
}

/// Identical alerts are emitted at most once per this many microseconds (10 seconds).
pub const ALERT_INTERVAL_MICROS: i64 = 10_000_000;
/// The most (tool, phase) pairs the rate limiter remembers; beyond it the stale entries go.
const ALERT_KEYS_MAX: usize = 1024;

impl TelemetryAlerts {
    /// A rate limiter with no history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Alerts held back by the rate limit so far.
    #[must_use]
    pub fn suppressed_total(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    /// `Some(n)` when the alert is to be emitted (`n` identical ones were held back since the
    /// last), `None` when it is suppressed. A poisoned lock emits: an alert is never lost to it.
    fn admit(&self, alert: &AuditAlert) -> Option<u64> {
        let Ok(mut state) = self.state.lock() else {
            return Some(0);
        };
        let key = (alert.tool.clone(), alert.phase);
        if let Some(entry) = state.get_mut(&key) {
            let elapsed = alert.at_micros.saturating_sub(entry.last_emitted);
            if (0..ALERT_INTERVAL_MICROS).contains(&elapsed) {
                entry.suppressed_since += 1;
                self.suppressed.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let held = entry.suppressed_since;
            entry.last_emitted = alert.at_micros;
            entry.suppressed_since = 0;
            return Some(held);
        }
        if state.len() >= ALERT_KEYS_MAX {
            state.retain(|_, e| {
                alert.at_micros.saturating_sub(e.last_emitted) < ALERT_INTERVAL_MICROS
            });
            if state.len() >= ALERT_KEYS_MAX {
                // Still full of live entries: emit rather than grow without bound.
                return Some(0);
            }
        }
        state.insert(
            key,
            AlertState {
                last_emitted: alert.at_micros,
                suppressed_since: 0,
            },
        );
        Some(0)
    }
}

impl AuditAlerts for TelemetryAlerts {
    fn alert(&self, alert: &AuditAlert) {
        let Some(suppressed) = self.admit(alert) else {
            return;
        };
        tracing::error!(
            target: "spot_mcp::audit",
            tool = %alert.tool,
            phase = ?alert.phase,
            policy = ?alert.policy,
            refused = alert.refused,
            suppressed_since_last = suppressed,
            "the MCP audit record could not be written"
        );
    }
}

/// How many failed records wait in memory for a retry.
pub const RETRY_QUEUE_CAPACITY: usize = 256;
/// How many times a queued record is retried before it is abandoned (and counted dropped).
pub const RETRY_MAX_ATTEMPTS: u32 = 8;
const BACKOFF_BASE_MICROS: i64 = 1_000_000;
const BACKOFF_CAP_MICROS: i64 = 60_000_000;

struct Pending {
    record: AuditRecord,
    attempts: u32,
    not_before: i64,
}

/// The audit-failure counters and the bounded in-memory retry queue.
pub struct AuditFailures {
    failures: AtomicU64,
    dropped: AtomicU64,
    queue: Mutex<VecDeque<Pending>>,
    capacity: usize,
}

impl Default for AuditFailures {
    fn default() -> Self {
        Self::with_capacity(RETRY_QUEUE_CAPACITY)
    }
}

impl AuditFailures {
    /// A queue holding at most `capacity` records (at least one).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            failures: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            queue: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
        }
    }

    /// `mcp_audit_failures_total`: failed audit writes, first attempts only.
    #[must_use]
    pub fn failures_total(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Records lost: pushed out of the full queue (oldest first) or abandoned after
    /// [`RETRY_MAX_ATTEMPTS`].
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Records waiting for a retry.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Counts one failed write and queues its record for a retry.
    pub fn note_failure(&self, record: AuditRecord, now_micros: i64) {
        self.count_failure();
        self.push(Pending {
            record,
            attempts: 0,
            not_before: now_micros.saturating_add(BACKOFF_BASE_MICROS),
        });
    }

    /// Counts one failed write whose record is not queued (the call was refused).
    pub fn note_refused(&self) {
        self.count_failure();
    }

    fn count_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(crate::metrics::AUDIT_FAILURES_TOTAL).increment(1);
    }

    fn count_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(crate::metrics::AUDIT_RECORDS_DROPPED_TOTAL).increment(1);
    }

    fn publish_depth(&self, depth: usize) {
        // A queue is at most a few hundred records: the cast to f64 is exact.
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::AUDIT_RETRY_QUEUE_DEPTH).set(depth as f64);
    }

    fn push(&self, pending: Pending) {
        let depth = if let Ok(mut queue) = self.queue.lock() {
            while queue.len() >= self.capacity {
                queue.pop_front();
                self.count_dropped();
            }
            queue.push_back(pending);
            queue.len()
        } else {
            return;
        };
        self.publish_depth(depth);
    }

    /// Retries the queued records that are due at `now_micros`. It stops at the first failure
    /// so a sink that is down is not hammered; a record that keeps failing backs off
    /// exponentially (bounded) and is abandoned after [`RETRY_MAX_ATTEMPTS`].
    pub async fn flush(&self, sink: &dyn AuditSink, now_micros: i64) {
        let due: Vec<Pending> = {
            let Ok(mut queue) = self.queue.lock() else {
                return;
            };
            let mut due = Vec::new();
            let mut keep = VecDeque::new();
            for pending in queue.drain(..) {
                if pending.not_before <= now_micros {
                    due.push(pending);
                } else {
                    keep.push_back(pending);
                }
            }
            *queue = keep;
            self.publish_depth(queue.len());
            due
        };
        let mut rest = due.into_iter();
        while let Some(mut pending) = rest.next() {
            if sink.record(pending.record.clone()).await.is_ok() {
                continue;
            }
            pending.attempts += 1;
            if pending.attempts >= RETRY_MAX_ATTEMPTS {
                self.count_dropped();
            } else {
                let shift = pending.attempts.min(20);
                let wait = (BACKOFF_BASE_MICROS << shift).min(BACKOFF_CAP_MICROS);
                pending.not_before = now_micros.saturating_add(wait);
                self.push(pending);
            }
            for later in rest {
                self.push(later);
            }
            return;
        }
    }
}

/// Builds the intent record for a write-capable call: the same shape as [`call_record`]
/// without an outcome, marked `phase = "intent"`.
#[must_use]
pub fn intent_record(principal: &McpPrincipal, tool: &str, at_micros: i64) -> AuditRecord {
    let mut record = call_record(principal, tool, true, at_micros);
    record.payload.remove("ok");
    record.payload.insert(
        PHASE_KEY.to_owned(),
        PayloadValue::Text(PHASE_INTENT.to_owned()),
    );
    record
}

/// Builds the audit record for one call.
#[must_use]
pub fn call_record(principal: &McpPrincipal, tool: &str, ok: bool, at_micros: i64) -> AuditRecord {
    let mut payload = Payload::new();
    payload.insert("tool".to_owned(), PayloadValue::Text(tool.to_owned()));
    payload.insert("ok".to_owned(), PayloadValue::Bool(ok));
    if let Some(tier) = principal.tier() {
        payload.insert(
            "tier".to_owned(),
            PayloadValue::Text(tier.as_str().to_owned()),
        );
    }
    if let Some(key) = principal.key_id() {
        payload.insert("api_key".to_owned(), PayloadValue::Text(key.to_owned()));
    }
    AuditRecord {
        kind: CALL_KIND.to_owned(),
        actor: principal.audit_actor(),
        on_behalf_of: None,
        subject: None,
        at: at_micros,
        payload,
        masked: true,
    }
}

/// The tier a tool needs: reads are T0, writes are T2, and an asynchronous structural call is
/// T3. A caller that is not an agent is not tier-checked.
#[must_use]
pub fn required_tier(writes: bool, async_job: bool) -> Tier {
    if async_job {
        Tier::T3
    } else if writes {
        Tier::T2
    } else {
        Tier::T0
    }
}
