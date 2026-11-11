//! The typed results of the journal operations.
//!
//! Results carry counts, positions, ids and reason codes. They never carry record content, a
//! key, a token or a personal value (ADR-061 sections 6 and 8). Instants are RFC 3339 text
//! (this crate reads no clock and takes no time dependency). Field names are the stable JSON
//! the management API returns.

use serde::{Deserialize, Serialize};

use crate::args::{OnUnreachable, PolicySetting, Position, RebuildMode};

/// The journal's health as the status shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalState {
    /// Shipping normally.
    Ok,
    /// Committing locally while the object store is unreachable (`remote-sync` with
    /// `degrade`); an RPO-breach alarm is raised.
    Degraded,
    /// Refusing commits (`remote-sync` with `halt` and the store unreachable).
    Halted,
    /// No journal target is configured: there is no off-site history.
    NoTarget,
}

/// `journal.status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResult {
    /// The journal's health.
    pub state: JournalState,
    /// The newest locally durable position.
    pub head: Option<Position>,
    /// The newest position durable off site.
    pub shipped: Option<Position>,
    /// How many sealed segments are not yet off site.
    pub unshipped_segments: u64,
    /// The tenant's policy.
    pub policy: PolicySetting,
}

/// The state of the key backup in the recovery report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyBackupState {
    /// A current key backup exists.
    Current,
    /// The key backup is older than the last key change.
    Stale,
    /// No key backup exists.
    Missing,
}

/// `journal.recovery.report`: the answer to "could I recover?" (ADR-061 section 8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReportResult {
    /// Whether a journal target is configured.
    pub target_configured: bool,
    /// The target's region code, when there is one.
    pub region: Option<String>,
    /// The tenant's policy.
    pub policy: PolicySetting,
    /// The last position a verify or audit proved.
    pub last_verified: Option<Position>,
    /// When that was (RFC 3339).
    pub last_verified_at: Option<String>,
    /// The oldest snapshot's id.
    pub oldest_snapshot: Option<String>,
    /// The key backup's state.
    pub key_backup: KeyBackupState,
    /// How many entities are excluded from capture (each has an owner's reason on the page).
    pub excluded_entities: u64,
}

/// One snapshot in a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// The snapshot's id.
    pub id: String,
    /// The journal position it covers.
    pub position: Position,
    /// When it was taken (RFC 3339).
    pub taken_at: String,
    /// Its size in bytes.
    pub bytes: u64,
}

/// `journal.snapshot.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotListResult {
    /// The snapshots, newest first.
    pub snapshots: Vec<SnapshotInfo>,
}

/// `journal.snapshot.take`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTakeResult {
    /// The new snapshot's id.
    pub id: String,
    /// The position it covers.
    pub position: Position,
}

/// `journal.verify`: the chain and the segments, checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyResult {
    /// Whether everything verified.
    pub ok: bool,
    /// How many segments were checked.
    pub segments_checked: u64,
    /// The first position that failed, when one did.
    pub first_failure: Option<Position>,
    /// A reason code for the failure (never content).
    pub reason: Option<String>,
}

/// How an audit ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditOutcome {
    /// No audit has run yet.
    NeverRun,
    /// The rebuilt state matched the live state.
    Match,
    /// The rebuilt state differed (`JOURNAL_AUDIT_MISMATCH`).
    Mismatch,
    /// The audit could not complete.
    Error,
}

/// `journal.audit.last` and `journal.audit.run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditResult {
    /// How it ended.
    pub outcome: AuditOutcome,
    /// When it ran (RFC 3339), if it ever did.
    pub ran_at: Option<String>,
    /// How many records were compared.
    pub records_compared: u64,
    /// The id of the first differing record, on a mismatch.
    pub first_difference: Option<String>,
    /// A reason code, on an error.
    pub reason: Option<String>,
}

/// `journal.policy.get` and `journal.policy.set`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyResult {
    /// The policy now in force.
    pub policy: PolicySetting,
    /// The policy before the call (`set` only).
    pub previous: Option<PolicySetting>,
}

impl PolicyResult {
    /// The `on_unreachable` choice in force, when the policy has one.
    #[must_use]
    pub fn on_unreachable(&self) -> Option<OnUnreachable> {
        match self.policy {
            PolicySetting::RemoteSync { on_unreachable } => Some(on_unreachable),
            _ => None,
        }
    }
}

/// `journal.reship`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReshipResult {
    /// How many segments were queued for shipping.
    pub queued: u64,
    /// How many are now off site.
    pub shipped: u64,
}

/// `journal.rebuild.plan`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildPlanResult {
    /// The hash a run must echo to execute exactly this plan (lower-case hex).
    pub plan_hash: String,
    /// The mode planned.
    pub mode: RebuildMode,
    /// The position the rebuild ends at.
    pub end: Position,
    /// The snapshot it starts from, if any.
    pub snapshot: Option<String>,
    /// How many segments it reads.
    pub segments: u64,
    /// How many records it applies.
    pub records: u64,
}

/// `journal.rebuild.run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildRunResult {
    /// The plan that ran.
    pub plan_hash: String,
    /// How many records were applied.
    pub records_applied: u64,
    /// The position reached.
    pub end: Position,
    /// Whether the post-rebuild verification passed.
    pub verified: bool,
}

/// One reader grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderInfo {
    /// Who holds the grant.
    pub grantee: String,
    /// Whether the grantee is outside the tenant.
    pub external: bool,
    /// When the grant expires (RFC 3339).
    pub expires_at: String,
}

/// `journal.reader.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderListResult {
    /// The current grants.
    pub readers: Vec<ReaderInfo>,
}

/// `journal.reader.grant`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderGrantResult {
    /// Who was granted access.
    pub grantee: String,
    /// When the grant expires (RFC 3339).
    pub expires_at: String,
}

/// `journal.reader.revoke`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderRevokeResult {
    /// Whose grant was revoked.
    pub grantee: String,
    /// Whether a grant existed and is now gone.
    pub revoked: bool,
}

/// `journal.subject.erase`. The subject is not echoed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectEraseResult {
    /// How many per-subject keys were destroyed.
    pub keys_destroyed: u64,
    /// How many fields now read as the documented placeholder.
    pub fields_erased: u64,
    /// How many fields were kept because a retention rule requires them.
    pub fields_retained: u64,
}
