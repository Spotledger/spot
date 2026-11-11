//! The inputs, by trait: what the control plane, the key service, the owners and the audit
//! history know about one tenant. Every method may fail; a failure is an unreadable fact, not a
//! pass.

use serde::{Deserialize, Serialize};
use spot_journal_audit::{
    DurabilityPolicy, ExclusionReason, JournalTarget, KeyBackupStatus, VerifiedPosition,
};

use crate::error::FactUnreadable;

/// The result of reading one input.
pub type FactResult<T> = Result<T, FactUnreadable>;

/// How often the tenant's journal is audited, which sets how old a verification may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    /// Audited daily.
    Daily,
    /// Audited weekly.
    Weekly,
}

/// What the control plane knows about the tenant's journal.
pub trait JournalFacts {
    /// The journal's object-store target and region; `None` when none is configured.
    ///
    /// # Errors
    /// [`FactUnreadable`] when the control plane cannot be read.
    fn journal_target(&self) -> FactResult<Option<JournalTarget>>;

    /// The durability policy; `None` when none is set or the control plane named one this build
    /// does not know.
    ///
    /// # Errors
    /// [`FactUnreadable`] when the control plane cannot be read.
    fn durability_policy(&self) -> FactResult<Option<DurabilityPolicy>>;

    /// Whether the tenant has any off-site journal history (a segment in the object store).
    ///
    /// # Errors
    /// [`FactUnreadable`] when it cannot be determined.
    fn has_offsite_history(&self) -> FactResult<bool>;

    /// How far the off-site journal trails the local commit, in milliseconds; `None` when it is
    /// not measured.
    ///
    /// # Errors
    /// [`FactUnreadable`] when it cannot be read.
    fn tail_lag_millis(&self) -> FactResult<Option<i64>>;

    /// How often the tenant is audited.
    ///
    /// # Errors
    /// [`FactUnreadable`] when it cannot be read.
    fn audit_cadence(&self) -> FactResult<Cadence>;
}

/// What the key service knows about the tenant's journal key.
pub trait KeyFacts {
    /// Whether the journal key is present.
    ///
    /// # Errors
    /// [`FactUnreadable`] when the key service cannot be read.
    fn journal_key_present(&self) -> FactResult<bool>;

    /// The key backup's status (with the time it was last verified).
    ///
    /// # Errors
    /// [`FactUnreadable`] when the key service cannot be read.
    fn key_backup(&self) -> FactResult<KeyBackupStatus>;
}

/// One entity a tenant does not recover from the journal, as its owner registered it.
///
/// The owner and the reason are what the owner stated; either may be absent, and an exclusion
/// with neither is a blocker, because nobody has said why the entity is out of the record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionEntry {
    /// The entity (the catalog's storage name).
    pub entity: String,
    /// The owner that excluded it (an app or module key); empty when none is recorded.
    pub owner: String,
    /// The reason the owner stated; `None` when none is recorded.
    pub reason: Option<ExclusionReason>,
}

impl ExclusionEntry {
    /// The order key: entity, owner, then the reason (none first).
    fn order_key(&self) -> (&str, &str, u8) {
        let reason = match self.reason {
            None => 0,
            Some(ExclusionReason::Derived) => 1,
            Some(ExclusionReason::LocalOnly) => 2,
        };
        (&self.entity, &self.owner, reason)
    }

    /// An exclusion with an owner and a reason.
    #[must_use]
    pub fn new(
        entity: impl Into<String>,
        owner: impl Into<String>,
        reason: ExclusionReason,
    ) -> Self {
        Self {
            entity: entity.into(),
            owner: owner.into(),
            reason: Some(reason),
        }
    }

    /// Whether the owner stated who excluded it and why.
    pub(crate) fn has_owner_reason(&self) -> bool {
        !self.owner.trim().is_empty() && self.reason.is_some()
    }
}

/// The registry of excluded entities, filled by the owners.
pub trait ExclusionRegistry {
    /// Every excluded entity of the tenant.
    ///
    /// # Errors
    /// [`FactUnreadable`] when the registry cannot be read.
    fn exclusions(&self) -> FactResult<Vec<ExclusionEntry>>;
}

/// The last audit that found the live and the rebuilt databases different.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFailure {
    /// When the audit ran, in unix milliseconds.
    pub at_millis: i64,
    /// How many records differed (a count, never a record).
    pub differing_records: u64,
}

/// The audit history of the tenant.
pub trait AuditHistory {
    /// The last position and time a journal audit verified clean; `None` when never.
    ///
    /// # Errors
    /// [`FactUnreadable`] when the history cannot be read.
    fn last_verified(&self) -> FactResult<Option<VerifiedPosition>>;

    /// The last audit that found a mismatch; `None` when none did.
    ///
    /// # Errors
    /// [`FactUnreadable`] when the history cannot be read.
    fn last_failure(&self) -> FactResult<Option<AuditFailure>>;
}

impl PartialOrd for ExclusionEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExclusionEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order_key().cmp(&other.order_key())
    }
}
