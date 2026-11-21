//! The report, the thresholds, the blockers and the verdict: plain data with a stable JSON form.
//!
//! The report holds **facts only** (what each input said, or that it could not be read); the
//! verdict is derived from it by [`crate::verdict`]. Nothing in either is free text or tenant
//! data: a reason is a catalogue key with parameters that are counts, ages and entity or owner
//! keys.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_journal_audit::{
    DurabilityPolicy, JournalTarget, KeyBackupStatus, OldestSnapshot, VerifiedPosition,
};

use crate::chain::ChainFacts;
use crate::facts::{AuditFailure, Cadence, ExclusionEntry};

/// One day, in milliseconds.
pub const DAY_MILLIS: i64 = 86_400_000;

/// An input's value, or the fact that it could not be read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Fact<T> {
    /// The input was read.
    Known(T),
    /// The input could not be read. The verdict cannot be a yes while this stands.
    Unreadable,
}

impl<T> Fact<T> {
    pub(crate) fn from_result<E>(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => Self::Known(value),
            Err(_) => Self::Unreadable,
        }
    }
}

/// The blob cross-check as the report keeps it: counts only, no hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSummary {
    /// How many referenced hashes were checked.
    pub checked: u64,
    /// How many of them the blob store does not hold.
    pub missing: u64,
}

/// The per-tenant recovery report (ADR-061 s8): every fact the verdict rests on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    /// The tenant the report is about.
    pub tenant: String,
    /// When the report was built, in unix milliseconds (the caller's now).
    pub generated_at_millis: i64,
    /// The journal target and region; `Known(None)` when none is configured.
    pub journal: Fact<Option<JournalTarget>>,
    /// The durability policy; `Known(None)` when none is set.
    pub durability_policy: Fact<Option<DurabilityPolicy>>,
    /// Whether the tenant has off-site journal history.
    pub offsite_history: Fact<bool>,
    /// How far the off-site journal trails the commit, in milliseconds.
    pub tail_lag_millis: Fact<Option<i64>>,
    /// How often the tenant is audited.
    pub audit_cadence: Fact<Cadence>,
    /// The last clean verification.
    pub last_verified: Fact<Option<VerifiedPosition>>,
    /// The last audit that found a mismatch.
    pub last_failure: Fact<Option<AuditFailure>>,
    /// Whether the snapshot listing could be read.
    pub snapshots_readable: bool,
    /// How many snapshots exist.
    pub snapshot_count: u64,
    /// How many of them a rebuild test has verified.
    pub snapshots_verified: u64,
    /// The oldest snapshot.
    pub oldest_snapshot: Option<OldestSnapshot>,
    /// Whether the journal key is present.
    pub journal_key_present: Fact<bool>,
    /// The key backup's status.
    pub key_backup: Fact<KeyBackupStatus>,
    /// Every excluded entity with its owner's reason, in entity order.
    pub excluded_entities: Fact<Vec<ExclusionEntry>>,
    /// The chain and manifest check.
    pub chain: ChainFacts,
    /// The blob cross-check; `None` when it did not run.
    pub blob_cross_check: Option<BlobSummary>,
}

/// The limits the verdict judges the report against. They are arguments: a deployment or a
/// tenant tier sets them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thresholds {
    /// The oldest a verification of a daily-audited tenant may be, in milliseconds.
    pub verification_max_age_daily_millis: i64,
    /// The oldest a verification of a weekly-audited tenant may be, in milliseconds.
    pub verification_max_age_weekly_millis: i64,
    /// The oldest the oldest snapshot may be, in milliseconds.
    pub snapshot_max_age_millis: i64,
    /// The oldest a key backup verification may be, in milliseconds.
    pub key_backup_max_age_millis: i64,
    /// The most the off-site journal may trail the commit, in milliseconds; `None` sets no limit.
    pub tail_lag_max_millis: Option<i64>,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            verification_max_age_daily_millis: 2 * DAY_MILLIS,
            verification_max_age_weekly_millis: 8 * DAY_MILLIS,
            snapshot_max_age_millis: 35 * DAY_MILLIS,
            key_backup_max_age_millis: 90 * DAY_MILLIS,
            tail_lag_max_millis: None,
        }
    }
}

/// Whether a blocker says "no" (a fact that is bad) or "cannot confirm" (a fact that is missing).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    /// An input read fine and says the tenant could not be recovered.
    Definite,
    /// An input is missing, unreadable or untrustworthy, so nothing can be confirmed.
    Unknown,
}

/// Why a tenant could not be recovered, or could not be confirmed recoverable. The variants are
/// declared in the stable order blockers are reported in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Blocker {
    /// The report was asked for without a tenant.
    TenantMissing,
    /// No journal target is configured.
    JournalTargetMissing,
    /// The journal's region is empty.
    RegionMissing,
    /// No durability policy is set.
    DurabilityPolicyMissing,
    /// Policy `local` and no off-site journal history: nothing survives the host.
    LocalPolicyWithoutOffsiteHistory,
    /// The off-site journal trails the commit by more than the limit.
    TailLagExceeded {
        /// The lag, in milliseconds.
        lag_millis: i64,
        /// The limit, in milliseconds.
        limit_millis: i64,
    },
    /// The journal was never verified.
    NeverVerified,
    /// The last verification is older than the tenant's cadence allows.
    VerificationStale {
        /// Its age in whole days.
        age_days: i64,
        /// The limit in whole days.
        limit_days: i64,
    },
    /// The last audit found a mismatch and no clean verification followed it.
    LastAuditMismatch {
        /// How many records differed.
        differing_records: u64,
    },
    /// No snapshot exists.
    NoSnapshot,
    /// The oldest snapshot is older than the limit.
    SnapshotTooOld {
        /// Its age in whole days.
        age_days: i64,
        /// The limit in whole days.
        limit_days: i64,
    },
    /// The journal key is not present.
    JournalKeyMissing,
    /// The journal key was never backed up.
    KeyBackupMissing,
    /// The last journal key backup attempt failed.
    KeyBackupFailed,
    /// The key backup was last verified longer ago than the limit.
    KeyBackupStale {
        /// Its age in whole days.
        age_days: i64,
        /// The limit in whole days.
        limit_days: i64,
    },
    /// The blob store lacks attachments the journal references.
    BlobsMissing {
        /// How many referenced hashes are missing.
        count: u64,
    },
    /// An excluded entity has no name.
    ExcludedEntityNameless,
    /// An excluded entity has no owner or no stated reason.
    ExcludedWithoutOwnerReason {
        /// The entity.
        entity: String,
    },
    /// A chain or manifest finding is evidence of tampering or loss.
    ChainTampering {
        /// The finding's `journal_verify.*` code.
        code: String,
        /// The generation it is about.
        generation: u64,
        /// The first position of its range.
        first_seq: u64,
        /// The last position of its range.
        last_seq: u64,
    },
    /// An input could not be read.
    InputUnreadable {
        /// The input's stable key.
        input: String,
    },
    /// An input is stamped in the future, so its age cannot be trusted.
    ClockSkew {
        /// The input's stable key.
        input: String,
    },
    /// A threshold is not positive, so the input it judges cannot be judged.
    ThresholdInvalid {
        /// The threshold's stable key.
        threshold: String,
    },
    /// The key backup's status is unknown.
    KeyBackupUnverified,
    /// The chain check covered only part of the journal.
    ChainCheckIncomplete,
    /// The blob cross-check did not run.
    BlobCrossCheckNotRun,
}

impl Blocker {
    /// The catalogue key of the reason (`recovery.blocker.<name>`).
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::TenantMissing => concat!("recovery", ".blocker.tenant_missing"),
            Self::JournalTargetMissing => concat!("recovery", ".blocker.journal_target_missing"),
            Self::RegionMissing => concat!("recovery", ".blocker.region_missing"),
            Self::DurabilityPolicyMissing => {
                concat!("recovery", ".blocker.durability_policy_missing")
            }
            Self::LocalPolicyWithoutOffsiteHistory => {
                concat!("recovery", ".blocker.local_policy_without_offsite_history")
            }
            Self::TailLagExceeded { .. } => concat!("recovery", ".blocker.tail_lag_exceeded"),
            Self::NeverVerified => concat!("recovery", ".blocker.never_verified"),
            Self::VerificationStale { .. } => concat!("recovery", ".blocker.verification_stale"),
            Self::LastAuditMismatch { .. } => concat!("recovery", ".blocker.last_audit_mismatch"),
            Self::NoSnapshot => concat!("recovery", ".blocker.no_snapshot"),
            Self::SnapshotTooOld { .. } => concat!("recovery", ".blocker.snapshot_too_old"),
            Self::JournalKeyMissing => concat!("recovery", ".blocker.journal_key_missing"),
            Self::KeyBackupMissing => concat!("recovery", ".blocker.key_backup_missing"),
            Self::KeyBackupFailed => concat!("recovery", ".blocker.key_backup_failed"),
            Self::KeyBackupStale { .. } => concat!("recovery", ".blocker.key_backup_stale"),
            Self::BlobsMissing { .. } => concat!("recovery", ".blocker.blobs_missing"),
            Self::ExcludedEntityNameless => {
                concat!("recovery", ".blocker.excluded_entity_nameless")
            }
            Self::ExcludedWithoutOwnerReason { .. } => {
                concat!("recovery", ".blocker.excluded_without_owner_reason")
            }
            Self::ChainTampering { .. } => concat!("recovery", ".blocker.chain_tampering"),
            Self::InputUnreadable { .. } => concat!("recovery", ".blocker.input_unreadable"),
            Self::ClockSkew { .. } => concat!("recovery", ".blocker.clock_skew"),
            Self::ThresholdInvalid { .. } => concat!("recovery", ".blocker.threshold_invalid"),
            Self::KeyBackupUnverified => concat!("recovery", ".blocker.key_backup_unverified"),
            Self::ChainCheckIncomplete => concat!("recovery", ".blocker.chain_check_incomplete"),
            Self::BlobCrossCheckNotRun => concat!("recovery", ".blocker.blob_cross_check_not_run"),
        }
    }

    /// Whether the blocker says "no" or "cannot confirm".
    #[must_use]
    pub fn kind(&self) -> BlockerKind {
        match self {
            Self::InputUnreadable { .. }
            | Self::ClockSkew { .. }
            | Self::ThresholdInvalid { .. }
            | Self::KeyBackupUnverified
            | Self::ChainCheckIncomplete
            | Self::BlobCrossCheckNotRun => BlockerKind::Unknown,
            _ => BlockerKind::Definite,
        }
    }

    /// The reason as the page and the API show it: the key and its parameters.
    #[must_use]
    pub fn reason(&self) -> BlockerReason {
        let mut params = BTreeMap::new();
        let mut put = |name: &str, value: String| {
            params.insert(name.to_owned(), value);
        };
        match self {
            Self::TailLagExceeded {
                lag_millis,
                limit_millis,
            } => {
                put("lag_millis", lag_millis.to_string());
                put("limit_millis", limit_millis.to_string());
            }
            Self::VerificationStale {
                age_days,
                limit_days,
            }
            | Self::SnapshotTooOld {
                age_days,
                limit_days,
            }
            | Self::KeyBackupStale {
                age_days,
                limit_days,
            } => {
                put("age_days", age_days.to_string());
                put("limit_days", limit_days.to_string());
            }
            Self::LastAuditMismatch { differing_records } => {
                put("differing_records", differing_records.to_string());
            }
            Self::BlobsMissing { count } => put("count", count.to_string()),
            Self::ExcludedWithoutOwnerReason { entity } => put("entity", entity.clone()),
            Self::ChainTampering {
                code,
                generation,
                first_seq,
                last_seq,
            } => {
                put("code", code.clone());
                put("generation", generation.to_string());
                put("first_seq", first_seq.to_string());
                put("last_seq", last_seq.to_string());
            }
            Self::InputUnreadable { input } | Self::ClockSkew { input } => {
                put("input", input.clone());
            }
            Self::ThresholdInvalid { threshold } => put("threshold", threshold.clone()),
            _ => {}
        }
        BlockerReason {
            key: self.key().to_owned(),
            kind: self.kind(),
            params,
        }
    }
}

/// A blocker as data: a catalogue key, whether it says no or cannot-confirm, and its parameters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerReason {
    /// The catalogue key (`recovery.blocker.key_backup_stale`).
    pub key: String,
    /// Whether it says no or cannot-confirm.
    pub kind: BlockerKind,
    /// The parameters of the key, as text.
    pub params: BTreeMap<String, String>,
}

/// Whether the tenant could be recovered now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Every input was read and every one says the tenant could be recovered.
    Recoverable,
    /// At least one input says the tenant could not be recovered.
    NotRecoverable {
        /// Every reason, in the stable order (definite and unknown together).
        blockers: Vec<BlockerReason>,
    },
    /// No input says no, but at least one is missing, unreadable or untrustworthy.
    CannotConfirm {
        /// Every reason, in the stable order.
        blockers: Vec<BlockerReason>,
    },
}

impl Verdict {
    /// Whether the tenant is confirmed recoverable.
    #[must_use]
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Recoverable)
    }

    /// Every reason, empty when recoverable.
    #[must_use]
    pub fn blockers(&self) -> &[BlockerReason] {
        match self {
            Self::Recoverable => &[],
            Self::NotRecoverable { blockers } | Self::CannotConfirm { blockers } => blockers,
        }
    }
}

/// The report and the verdict drawn from it, the pair a console shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    /// The facts.
    pub report: RecoveryReport,
    /// What they add up to.
    pub verdict: Verdict,
}
