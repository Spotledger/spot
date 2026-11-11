//! The names of the audit-failure metrics, published through the `metrics` facade that
//! `spot-telemetry` renders. They carry no label: a tool name or a value would be unbounded.

/// Counter: failed audit writes, first attempts only.
pub const AUDIT_FAILURES_TOTAL: &str = "mcp_audit_failures_total";
/// Counter: audit records lost (pushed out of a full retry queue or abandoned).
pub const AUDIT_RECORDS_DROPPED_TOTAL: &str = "mcp_audit_records_dropped_total";
/// Gauge: audit records waiting for a retry.
pub const AUDIT_RETRY_QUEUE_DEPTH: &str = "mcp_audit_retry_queue_depth";
