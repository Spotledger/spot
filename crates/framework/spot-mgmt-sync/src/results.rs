//! The typed results of the sync operations.
//!
//! Results carry counts, positions, ids and reason codes. They never carry record content, a
//! key, a token or a personal value (ADR-067 section 6). The one exception is the enrolment
//! code in [`EnrolResult`], which is shown once and wrapped so it cannot reach a log. Instants
//! are RFC 3339 text (this crate reads no clock and takes no time dependency). The health
//! result is [`spot_sync_health::HealthView`] itself, not a copy.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::args::{NodeRole, Position};

/// A node's lifecycle state (ADR-062).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeLifecycle {
    /// Enrolled, the one-time code not yet redeemed.
    Pending,
    /// Syncing.
    Active,
    /// Deliberately suspended by an operator.
    Suspended,
    /// Revoked for good.
    Revoked,
}

/// One node in a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSummary {
    /// The node's name.
    pub node: String,
    /// Its role.
    pub role: NodeRole,
    /// Its lifecycle state.
    pub state: NodeLifecycle,
    /// When it last made contact (RFC 3339), if ever.
    pub last_contact: Option<String>,
    /// The software version it reported, if any.
    pub version: Option<String>,
}

/// `sync.node.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeListResult {
    /// The tenant's nodes, by name.
    pub nodes: Vec<NodeSummary>,
}

/// `sync.node.show`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeShowResult {
    /// The node's summary.
    pub node: NodeSummary,
    /// The partitions it receives, by name.
    pub partitions: Vec<String>,
    /// The subscription's name or id.
    pub subscription: String,
    /// When its certificate expires (RFC 3339), if it has one.
    pub certificate_expires_at: Option<String>,
    /// How many positions it is behind.
    pub lag_positions: u64,
    /// The newest position it acknowledged.
    pub acknowledged: Option<Position>,
}

/// The one-time enrolment code. Its `Debug` prints `<redacted>`; it serialises as the bare
/// text exactly once, into the result the caller reads at the prompt, and is never stored,
/// logged or audited (the audit record holds the enrolment id only).
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct OneTimeCode(String);

impl OneTimeCode {
    /// Wraps a freshly issued code. Only the executor creates one.
    #[must_use]
    pub fn new(code: String) -> Self {
        Self(code)
    }

    /// The code, for the prompt that shows it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OneTimeCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// `sync.node.enrol`.
#[derive(Debug, Serialize, Deserialize)]
pub struct EnrolResult {
    /// The enrolment's id: what the audit record holds.
    pub enrolment_id: String,
    /// The node being enrolled.
    pub node: String,
    /// When the code stops working (RFC 3339).
    pub expires_at: String,
    /// The one-time code, shown once. Absent from any stored copy of the result (the stored
    /// copy replaces it), so it is skipped when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<OneTimeCode>,
}

/// The result of `sync.node.suspend`, `sync.node.resume` and `sync.node.revoke`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStateResult {
    /// The node.
    pub node: String,
    /// Its state after the operation.
    pub state: NodeLifecycle,
}

/// `sync.node.rotate_certificate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotateCertificateResult {
    /// The node.
    pub node: String,
    /// When the new certificate expires (RFC 3339).
    pub certificate_expires_at: String,
}

/// `sync.node.resend`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResendResult {
    /// The node.
    pub node: String,
    /// The position sending resumes from.
    pub from: Position,
    /// How many positions are queued.
    pub queued_positions: u64,
}

/// `sync.node.notify_contact`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyContactResult {
    /// The node.
    pub node: String,
    /// Whether a contact was found and notified.
    pub notified: bool,
}

/// How serious an open condition is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionSeverity {
    /// Informational.
    Info,
    /// Minor or being handled.
    Amber,
    /// A person is needed.
    Red,
}

/// One open condition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionRow {
    /// The catalogue id of the condition.
    pub condition: String,
    /// Its current severity.
    pub severity: ConditionSeverity,
    /// The kind of subject it is about.
    pub subject_kind: String,
    /// The subject: an id, never tenant data (already redacted by the raiser).
    pub subject: String,
    /// When it opened (RFC 3339).
    pub since: String,
}

/// `sync.condition.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionListResult {
    /// The open conditions, worst first.
    pub conditions: Vec<ConditionRow>,
}

/// One quarantined item: ids and a reason code, never the record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRow {
    /// The item's id.
    pub item: String,
    /// The node it came from.
    pub source: String,
    /// Its position in the source's stream.
    pub position: Position,
    /// Why it is quarantined (a stable code).
    pub reason_code: String,
    /// Since when (RFC 3339).
    pub since: String,
}

/// `sync.quarantine.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineListResult {
    /// The items, oldest first.
    pub items: Vec<QuarantineRow>,
}

/// What a retry did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryOutcome {
    /// The item applied and left quarantine.
    Applied,
    /// It failed again and stays quarantined.
    StillQuarantined,
}

/// `sync.quarantine.retry`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRetryResult {
    /// The item.
    pub item: String,
    /// What the retry did.
    pub outcome: RetryOutcome,
}

/// `sync.quarantine.discard`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineDiscardResult {
    /// The item.
    pub item: String,
    /// Whether it was discarded (false when it was already closed).
    pub discarded: bool,
}

/// `sync.policy.show`: the preset and its settings as names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyShowResult {
    /// The doctype.
    pub doctype: String,
    /// The preset's name (ADR-063).
    pub preset: String,
    /// The adoption mode's name, if the preset has one.
    pub adopt: Option<String>,
}

/// One finding of the policy lint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    /// The lint rule's code (for example `l2`).
    pub rule: String,
    /// The doctype it is about.
    pub doctype: String,
}

/// `sync.policy.lint`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLintResult {
    /// Whether there are no findings.
    pub clean: bool,
    /// The findings.
    pub findings: Vec<LintFinding>,
}

/// `sync.role.change`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleChangeResult {
    /// The role before.
    pub from: NodeRole,
    /// The role after.
    pub to: NodeRole,
}

/// How far the bootstrap has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapState {
    /// Not started.
    NotStarted,
    /// Running.
    Running,
    /// Done.
    Complete,
    /// Stopped by a failure.
    Failed,
}

/// `sync.bootstrap.status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapStatusResult {
    /// The state.
    pub state: BootstrapState,
    /// How many ranges the bootstrap covers.
    pub ranges_total: u64,
    /// How many are done.
    pub ranges_done: u64,
    /// The newest position applied.
    pub position: Option<Position>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_never_debug_prints_and_is_skipped_when_absent() {
        let code = OneTimeCode::new("abcd-efgh-ijkl".to_owned());
        assert_eq!(format!("{code:?}"), "<redacted>");
        let result = EnrolResult {
            enrolment_id: "enr-1".into(),
            node: "branch07".into(),
            expires_at: "2026-11-13T00:00:00Z".into(),
            code: Some(code),
        };
        assert!(!format!("{result:?}").contains("abcd"));
        let shown = serde_json::to_value(&result).unwrap_or_default();
        assert_eq!(shown["code"], "abcd-efgh-ijkl");
        let stored = EnrolResult {
            code: None,
            ..result
        };
        let stored = serde_json::to_value(&stored).unwrap_or_default();
        assert!(stored.get("code").is_none());
    }
}
