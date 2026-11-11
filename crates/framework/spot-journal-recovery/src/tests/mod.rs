//! Tests: one per blocker, the healthy tenant, threshold boundaries, ordering, fail-closed reads,
//! the store-backed inputs and the golden JSON.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use spot_journal_audit::{
    BlobCrossCheck, DurabilityPolicy, ExclusionReason, JournalTarget, KeyBackupStatus, Position,
    VerifiedPosition,
};

use crate::{
    AuditFailure, AuditHistory, Cadence, ChainFacts, ChainFinding, ChainState, DAY_MILLIS,
    ExclusionEntry, ExclusionRegistry, FactResult, FactUnreadable, JournalFacts, KeyFacts,
    RecoveryInputs, SnapshotFacts, Thresholds,
};

mod blockers;
mod golden;
mod store_inputs;

pub(crate) const NOW: i64 = 100 * DAY_MILLIS;

/// A fully configurable set of facts. A `None` in an `Option<Result>`-like field is an
/// unreadable input; the plain fields are what a read returns.
#[derive(Clone)]
pub(crate) struct Fixture {
    pub tenant: String,
    pub target: Option<JournalTarget>,
    pub policy: Option<DurabilityPolicy>,
    pub offsite: bool,
    pub lag: Option<i64>,
    pub cadence: Cadence,
    pub verified: Option<VerifiedPosition>,
    pub failure: Option<AuditFailure>,
    pub key_present: bool,
    pub key_backup: KeyBackupStatus,
    pub exclusions: Vec<ExclusionEntry>,
    pub chain: ChainFacts,
    pub snapshots: SnapshotFacts,
    pub blobs: Option<BlobCrossCheck>,
    /// Inputs that fail to read, by key.
    pub broken: Vec<&'static str>,
    /// Tenant data that must never reach the report. It lives in the fixture only to prove it.
    pub tenant_data: Vec<String>,
}

impl Fixture {
    pub fn healthy() -> Self {
        Self {
            tenant: "acme".to_owned(),
            target: Some(JournalTarget {
                target: "journal-bucket/acme".to_owned(),
                region: "eu-central".to_owned(),
            }),
            policy: Some(DurabilityPolicy::RemoteSync),
            offsite: true,
            lag: Some(500),
            cadence: Cadence::Weekly,
            verified: Some(VerifiedPosition {
                position: Position::new(1, 4200),
                at_millis: NOW - DAY_MILLIS,
            }),
            failure: None,
            key_present: true,
            key_backup: KeyBackupStatus::Confirmed {
                verified_at_millis: NOW - 5 * DAY_MILLIS,
            },
            exclusions: vec![
                ExclusionEntry::new("report_cache", "platform", ExclusionReason::Derived),
                ExclusionEntry::new("device_pairing", "edge", ExclusionReason::LocalOnly),
            ],
            chain: ChainFacts::new("acme", ChainState::Complete, []),
            snapshots: SnapshotFacts {
                readable: true,
                count: 3,
                verified: 2,
                oldest: Some(spot_journal_audit::OldestSnapshot {
                    position: Position::new(1, 100),
                    taken_at_millis: NOW - 20 * DAY_MILLIS,
                }),
            },
            blobs: Some(BlobCrossCheck::clean(12)),
            broken: Vec::new(),
            tenant_data: vec![
                "Acme Traders Ltd".to_owned(),
                "INV-2026-000417".to_owned(),
                "4111111111111111".to_owned(),
            ],
        }
    }

    fn read<T>(&self, input: &'static str, value: T) -> FactResult<T> {
        if self.broken.contains(&input) {
            Err(FactUnreadable::new(input))
        } else {
            Ok(value)
        }
    }

    pub fn inputs(&self) -> RecoveryInputs<'_> {
        RecoveryInputs {
            tenant: &self.tenant,
            journal: self,
            keys: self,
            exclusions: self,
            history: self,
            chain: self.chain.clone(),
            snapshots: self.snapshots,
            blobs: self.blobs.clone(),
        }
    }
}

impl JournalFacts for Fixture {
    fn journal_target(&self) -> FactResult<Option<JournalTarget>> {
        self.read("journal_target", self.target.clone())
    }
    fn durability_policy(&self) -> FactResult<Option<DurabilityPolicy>> {
        self.read("durability_policy", self.policy)
    }
    fn has_offsite_history(&self) -> FactResult<bool> {
        self.read("offsite_history", self.offsite)
    }
    fn tail_lag_millis(&self) -> FactResult<Option<i64>> {
        self.read("tail_lag", self.lag)
    }
    fn audit_cadence(&self) -> FactResult<Cadence> {
        self.read("audit_cadence", self.cadence)
    }
}

impl KeyFacts for Fixture {
    fn journal_key_present(&self) -> FactResult<bool> {
        self.read("journal_key", self.key_present)
    }
    fn key_backup(&self) -> FactResult<KeyBackupStatus> {
        self.read("key_backup", self.key_backup)
    }
}

impl ExclusionRegistry for Fixture {
    fn exclusions(&self) -> FactResult<Vec<ExclusionEntry>> {
        self.read("exclusions", self.exclusions.clone())
    }
}

impl AuditHistory for Fixture {
    fn last_verified(&self) -> FactResult<Option<VerifiedPosition>> {
        self.read("last_verified", self.verified)
    }
    fn last_failure(&self) -> FactResult<Option<AuditFailure>> {
        self.read("last_failure", self.failure)
    }
}

pub(crate) fn tampering(code: &str, first: u64, last: u64) -> ChainFinding {
    ChainFinding {
        code: code.to_owned(),
        generation: 1,
        first_seq: first,
        last_seq: last,
        tampering: true,
    }
}

pub(crate) fn defaults() -> Thresholds {
    Thresholds::default()
}
