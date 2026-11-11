//! The audit of refused operations (WP-SEC-06.T3).
//!
//! A refused operation is the most interesting thing to an auditor and it leaves no write, so the
//! participant never runs for it. The document engine tells a [`DenialSink`] instead, after the
//! operation's transaction has rolled back; [`AuditDenialSink`] writes one small `sys_doc_audit`
//! event with `outcome = "denied"` through the [`AuditWriter`], which must open **its own
//! transaction** (the business one is gone).
//!
//! # What a denial event holds
//!
//! The access-attempt trail: who (the actor), what (the DocType, the id when the request named
//! one, the operation and its cause), when, and why it was refused (the error's class and code).
//! It holds **no diff, no field value and no message**: the actor was trying to write something
//! it may not write, and the message may contain it. The engine already strips the message and
//! details from the error it hands over; this sink reads only the class and the code, and keeps
//! a code only when it looks like one.
//!
//! # A flood of denials
//!
//! Anyone can cause a denial, so the path is bounded:
//!
//! - one small event in one transaction per denial;
//! - at most [`AuditDenialSink::max_in_flight`] writes at a time; a denial over the limit is
//!   dropped and counted ([`DenialStats::dropped`], surfaced as
//!   `DocAudit::status().denials_dropped`), so a flood never queues work behind the business
//!   path;
//! - the engine time-boxes the sink and never waits on it for the operation's answer.
//!
//! # A denial that cannot be recorded
//!
//! Unlike a successful write, a refusal stands whether or not it is recorded, so the sink cannot
//! fail the operation. It must not be silent either: the failure is counted
//! ([`DenialStats::not_recorded`], `DocAudit::status().denials_not_recorded`) and logged once
//! under `docaudit.denial_not_recorded` (never with the cause: it may name a value).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use spot_app_api::BoxFuture;
use spot_document::engine::{Denial, DenialSink};

use crate::error::DENIAL_NOT_RECORDED;
use crate::event::{
    ActorRef, AuditSource, DocAuditEvent, DocOperation, EventExtra, OUTCOME_DENIED,
};
use crate::participant::operation_for;
use crate::step::{AuditWriter, is_audit_off, micros};

/// The default bound on denial writes in flight at once.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 8;

/// The subject id of a denial whose request named no document. A valid UUIDv7 shape so the
/// outbox accepts it; it names no real document.
pub const UNKNOWN_ID: &str = "00000000-0000-7000-8000-000000000000";

/// The longest error code a denial event keeps, in bytes.
const MAX_CODE_BYTES: usize = 128;

/// What the denial path did, for the audit status. Shared between the sink and
/// [`DocAudit`](crate::DocAudit).
#[derive(Debug, Default)]
pub struct DenialStats {
    recorded: AtomicU64,
    dropped: AtomicU64,
    not_recorded: AtomicU64,
    logged: AtomicBool,
}

impl DenialStats {
    /// Fresh counters, shared.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Denials written to the audit.
    #[must_use]
    pub fn recorded(&self) -> u64 {
        self.recorded.load(Ordering::Relaxed)
    }

    /// Denials dropped because too many writes were in flight.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Denials the writer could not record.
    #[must_use]
    pub fn not_recorded(&self) -> u64 {
        self.not_recorded.load(Ordering::Relaxed)
    }

    fn failed(&self) {
        self.not_recorded.fetch_add(1, Ordering::Relaxed);
        // Once, and only the fact: the cause can name a value.
        if !self.logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                code = DENIAL_NOT_RECORDED,
                "a refused operation could not be recorded in the document audit; further failures are only counted"
            );
        }
    }
}

/// Records every refused operation as a `denied` audit event.
pub struct AuditDenialSink {
    writer: Arc<dyn AuditWriter>,
    stats: Arc<DenialStats>,
    source: AuditSource,
    node: Option<String>,
    max_in_flight: usize,
    in_flight: AtomicUsize,
}

impl AuditDenialSink {
    /// A sink writing through `writer` (which opens its own transaction per event) and counting
    /// into `stats`.
    #[must_use]
    pub fn new(writer: Arc<dyn AuditWriter>, stats: Arc<DenialStats>) -> Self {
        Self {
            writer,
            stats,
            source: AuditSource::Api,
            node: None,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Sets where refused operations are recorded as coming from.
    #[must_use]
    pub fn with_source(mut self, source: AuditSource) -> Self {
        self.source = source;
        self
    }

    /// Records `node` (ADR-062) on every denial; the engine does not hand a denial its node.
    #[must_use]
    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = Some(node.into());
        self
    }

    /// Sets the bound on writes in flight (at least one).
    #[must_use]
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_in_flight = max.max(1);
        self
    }

    /// The bound on writes in flight.
    #[must_use]
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> &Arc<DenialStats> {
        &self.stats
    }

    /// The event for one refusal; `None` for a DocType on the audit-off list.
    #[must_use]
    pub fn build(&self, denial: &Denial<'_>) -> Option<DocAuditEvent> {
        if is_audit_off(denial.doctype) {
            return None;
        }
        let (operation, mut extra): (DocOperation, EventExtra) =
            operation_for(denial.operation, denial.cause);
        extra.node = self.node.clone();
        extra.outcome = Some(OUTCOME_DENIED.to_owned());
        extra.error_class = Some(denial.error.class.api_code().to_owned());
        extra.error_code = Some(code_of(&denial.error.code));
        Some(DocAuditEvent {
            site: denial.invocation.site.as_str().to_owned(),
            doctype: denial.doctype.to_owned(),
            id: denial
                .id
                .map_or_else(|| UNKNOWN_ID.to_owned(), |id| id.to_string()),
            version: 0,
            operation,
            actor: ActorRef::of(&denial.invocation.actor),
            at: micros(denial.invocation.now),
            changed_fields: Vec::new(),
            diff: Vec::new(),
            hidden_changes: false,
            command_id: denial
                .invocation
                .command_id
                .as_ref()
                .map(|c| c.as_str().to_owned()),
            source: self.source,
            truncated: false,
            diff_digest: None,
            method: extra.method,
            correction_half: extra.correction_half,
            via: extra.via,
            node: extra.node,
            outcome: extra.outcome,
            error_class: extra.error_class,
            error_code: extra.error_code,
        })
    }

    /// Takes a write slot, or `None` when the bound is reached.
    fn acquire(&self) -> Option<Slot<'_>> {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current >= self.max_in_flight {
                return None;
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Slot(&self.in_flight)),
                Err(seen) => current = seen,
            }
        }
    }

    /// Writes one prepared event, bounded; counts the outcome.
    fn record(&self, event: &DocAuditEvent) {
        let Some(_slot) = self.acquire() else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match self.writer.write(event) {
            Ok(()) => {
                self.stats.recorded.fetch_add(1, Ordering::Relaxed);
            }
            // Only the fact is kept: the writer's explanation can name a value.
            Err(_) => self.stats.failed(),
        }
    }
}

impl DenialSink for AuditDenialSink {
    fn denied(&self, denial: &Denial<'_>) -> BoxFuture<'_, ()> {
        // Built now: the borrowed denial does not outlive this call.
        let event = self.build(denial);
        Box::pin(async move {
            if let Some(event) = event {
                self.record(&event);
            }
        })
    }
}

/// Releases a write slot, also when the writer panics.
struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The error code, kept only when it is a bounded dotted lower-case code.
fn code_of(code: &str) -> String {
    let ok = !code.is_empty()
        && code.len() <= MAX_CODE_BYTES
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
    if ok {
        code.to_owned()
    } else {
        "unknown".to_owned()
    }
}
