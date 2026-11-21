//! The per-tenant recovery report (ADR-061 s8 and s9): the answer to "could I recover this
//! tenant, right now?".
//!
//! The report lists the journal target and its region, the durability policy, the last verified
//! position and time, the oldest snapshot, the key backup status, every excluded entity with its
//! owner and reason, and the blob cross-check. Its [`could_recover`](RecoveryReport::could_recover)
//! is `No` as soon as one input says so, and then names **every** reason, not just the first.
//!
//! It reads no clock and no store: the caller supplies the inputs and the current time, so a
//! report is reproducible and testable. Nothing is free text — a tenant, an entity, an owner and a
//! region are keys, and a reason is a [`Blocker`] variant, so the page that shows it and the test
//! that proves it read the same values.

use serde::{Deserialize, Serialize};

/// A journal position: the stream generation and the sequence number within it (ADR-053).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    /// The stream generation.
    pub generation: u64,
    /// The sequence number within the generation.
    pub seq: u64,
}

impl Position {
    /// A position.
    #[must_use]
    pub fn new(generation: u64, seq: u64) -> Self {
        Self { generation, seq }
    }
}

/// Where the journal lives: its object-store target and the region it is kept in (ADR-058).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalTarget {
    /// The object-store target of the tenant's journal (bucket and prefix).
    pub target: String,
    /// The region the journal is kept in.
    pub region: String,
}

/// The tenant's durability policy (ADR-061 s4), a per-tenant control-plane setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DurabilityPolicy {
    /// A local durable commit only.
    Local,
    /// A synchronous standby that has the commit.
    ReplicaSync,
    /// The entry is durable in the journal's object store before the commit is acknowledged.
    RemoteSync,
}

impl DurabilityPolicy {
    /// The policy's name, as the control plane and the report spell it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ReplicaSync => "replica-sync",
            Self::RemoteSync => "remote-sync",
        }
    }

    /// The policy a name names, or `None` when the control plane reported something this build
    /// does not know — which the report treats as an unknown policy, not as a safe default.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "replica-sync" => Some(Self::ReplicaSync),
            "remote-sync" => Some(Self::RemoteSync),
            _ => None,
        }
    }
}

/// The position the journal was last verified at, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedPosition {
    /// The position that was verified.
    pub position: Position,
    /// When it was verified, in unix milliseconds.
    pub at_millis: i64,
}

/// The oldest snapshot a rebuild can start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OldestSnapshot {
    /// The position the snapshot is at.
    pub position: Position,
    /// When the snapshot was taken, in unix milliseconds.
    pub taken_at_millis: i64,
}

/// The journal key backup's status (ADR-061 s9: a journal without its keys is unreadable by
/// design, so its backup is part of the recovery plan).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KeyBackupStatus {
    /// A backup exists and was verified at that time.
    Confirmed {
        /// When the backup was last verified, in unix milliseconds.
        verified_at_millis: i64,
    },
    /// No backup was ever taken.
    Never,
    /// A backup was attempted and failed.
    Failed,
    /// Nothing is known about the backup.
    #[default]
    Unverified,
}

/// Why an entity is out of the recoverable record (ADR-061 s3). The owner states the reason; the
/// platform carries it, it does not invent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// The catalog flags the entity `derived`: a rebuild makes its projector fill it.
    Derived,
    /// The owner flags the entity `local` (ADR-063): it is not part of the recoverable record.
    LocalOnly,
}

/// One entity the tenant does not recover from the journal, with the owner that excluded it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedEntity {
    /// The entity (the catalog's storage name).
    pub entity: String,
    /// The owner that excluded it (an app or module key).
    pub owner: String,
    /// Why it is excluded.
    pub reason: ExclusionReason,
}

impl ExcludedEntity {
    /// An exclusion.
    #[must_use]
    pub fn new(
        entity: impl Into<String>,
        owner: impl Into<String>,
        reason: ExclusionReason,
    ) -> Self {
        Self {
            entity: entity.into(),
            owner: owner.into(),
            reason,
        }
    }
}

/// The blob cross-check (ADR-061 s9): every attachment hash the journal references must exist in
/// the blob store, and the ones that do not are named.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobCrossCheck {
    /// How many referenced hashes were checked.
    pub checked: u64,
    /// The referenced hashes the blob store does not hold, sorted and without repeats.
    pub missing: Vec<String>,
}

impl BlobCrossCheck {
    /// A cross-check that found nothing missing.
    #[must_use]
    pub fn clean(checked: u64) -> Self {
        Self {
            checked,
            missing: Vec::new(),
        }
    }

    /// A cross-check with the referenced hashes the blob store does not hold; they are sorted and
    /// deduplicated so the report is the same whatever order the check found them in.
    #[must_use]
    pub fn new<S: Into<String>>(checked: u64, missing: impl IntoIterator<Item = S>) -> Self {
        let mut missing: Vec<String> = missing.into_iter().map(Into::into).collect();
        missing.sort();
        missing.dedup();
        Self { checked, missing }
    }

    /// Whether every referenced hash exists.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Why a tenant could not be recovered from its journal. Each variant is one input that says no.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Blocker {
    /// The report was asked for without a tenant.
    TenantMissing,
    /// The journal target is unknown or empty.
    JournalTargetMissing,
    /// The journal region is unknown or empty, so the journal cannot be located under ADR-058.
    RegionMissing,
    /// The durability policy is unknown or absent.
    DurabilityPolicyUnknown,
    /// The journal was never verified by an audit.
    NeverVerified,
    /// The verification window is missing or not positive, so "verified" cannot be judged.
    VerificationWindowInvalid,
    /// The last verification is stamped after the caller's now, so it cannot be trusted.
    VerifiedAtInTheFuture,
    /// The last verification is older than the window the tenant set.
    VerificationStale {
        /// How old the verification is, in milliseconds.
        age_millis: i64,
        /// The window it should be within, in milliseconds.
        limit_millis: i64,
    },
    /// No snapshot exists, so a rebuild has no base image to start from.
    NoSnapshot,
    /// The journal key was never backed up.
    KeyBackupNever,
    /// The journal key backup failed.
    KeyBackupFailed,
    /// The journal key backup's status is unknown.
    KeyBackupUnverified,
    /// An excluded entity has no name.
    ExcludedEntityNameless,
    /// An excluded entity names no owner, so nobody has stated why it is out of the record.
    ExcludedWithoutOwner {
        /// The entity that names no owner.
        entity: String,
    },
    /// The blob cross-check did not run, so missing attachments are unknown.
    BlobCrossCheckNotRun,
    /// The blob cross-check found referenced hashes the blob store does not hold.
    BlobsMissing {
        /// How many referenced hashes are missing.
        count: u64,
    },
}

/// Whether the tenant could be recovered, and every reason it could not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Every input says the tenant could be recovered now.
    Yes,
    /// At least one input says it could not; every reason is named.
    No {
        /// The reasons, in the order the report checks them.
        reasons: Vec<Blocker>,
    },
}

impl Verdict {
    /// Whether the tenant could be recovered.
    #[must_use]
    pub fn is_yes(&self) -> bool {
        matches!(self, Self::Yes)
    }

    /// The reasons it could not, empty when it could.
    #[must_use]
    pub fn reasons(&self) -> &[Blocker] {
        match self {
            Self::Yes => &[],
            Self::No { reasons } => reasons,
        }
    }
}

/// The per-tenant recovery report (ADR-061 s8): what the journal holds, what it does not, and
/// therefore whether the tenant could be recovered now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    /// The tenant the report is about.
    pub tenant: String,
    /// Where the journal lives.
    pub journal: JournalTarget,
    /// The tenant's durability policy, when the control plane reported a known one.
    pub durability_policy: Option<DurabilityPolicy>,
    /// The last verified position and time, when the journal was verified.
    pub last_verified: Option<VerifiedPosition>,
    /// The oldest snapshot a rebuild could start from.
    pub oldest_snapshot: Option<OldestSnapshot>,
    /// The journal key backup's status.
    pub key_backup: KeyBackupStatus,
    /// The entities the tenant does not recover from the journal, in entity order.
    pub excluded_entities: Vec<ExcludedEntity>,
    /// The blob cross-check, when it ran.
    pub blob_cross_check: Option<BlobCrossCheck>,
    /// Whether the tenant could be recovered, with every reason it could not.
    pub could_recover: Verdict,
}

impl RecoveryReport {
    /// Whether the tenant could be recovered.
    #[must_use]
    pub fn could_recover(&self) -> bool {
        self.could_recover.is_yes()
    }

    /// Every reason the tenant could not be recovered, empty when it could.
    #[must_use]
    pub fn blockers(&self) -> &[Blocker] {
        self.could_recover.reasons()
    }
}

/// Builds a [`RecoveryReport`] from the inputs a tenant's control plane and audit supply.
///
/// Nothing is refused: an input that is absent or unusable degrades
/// [`could_recover`](RecoveryReport::could_recover) to `No` with a [`Blocker`] that names it, so a
/// half-configured tenant still gets a page that says what is missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReportBuilder {
    /// The tenant.
    tenant: String,
    /// The journal's object-store target.
    target: String,
    /// The journal's region.
    region: String,
    /// The durability policy.
    policy: Option<DurabilityPolicy>,
    /// The last verified position and time.
    verified: Option<VerifiedPosition>,
    /// How old a verification may be, in milliseconds.
    verified_within_millis: Option<i64>,
    /// The oldest snapshot.
    snapshot: Option<OldestSnapshot>,
    /// The journal key backup's status.
    key_backup: KeyBackupStatus,
    /// The excluded entities.
    excluded: Vec<ExcludedEntity>,
    /// The blob cross-check.
    blobs: Option<BlobCrossCheck>,
}

impl RecoveryReportBuilder {
    /// A builder for one tenant.
    #[must_use]
    pub fn new(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            ..Self::default()
        }
    }

    /// The journal's object-store target and region.
    #[must_use]
    pub fn journal_target(mut self, target: impl Into<String>, region: impl Into<String>) -> Self {
        self.target = target.into();
        self.region = region.into();
        self
    }

    /// The tenant's durability policy.
    #[must_use]
    pub fn durability_policy(mut self, policy: DurabilityPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// The last verified position and time, and how old such a verification may be before the
    /// report calls it stale. The window is the tenant's setting, so it is an argument.
    #[must_use]
    pub fn last_verified(mut self, position: Position, at_millis: i64, within_millis: i64) -> Self {
        self.verified = Some(VerifiedPosition {
            position,
            at_millis,
        });
        self.verified_within_millis = Some(within_millis);
        self
    }

    /// The oldest snapshot a rebuild could start from.
    #[must_use]
    pub fn oldest_snapshot(mut self, position: Position, taken_at_millis: i64) -> Self {
        self.snapshot = Some(OldestSnapshot {
            position,
            taken_at_millis,
        });
        self
    }

    /// The journal key backup's status.
    #[must_use]
    pub fn key_backup(mut self, status: KeyBackupStatus) -> Self {
        self.key_backup = status;
        self
    }

    /// One entity the tenant does not recover from the journal, with the owner that excluded it.
    #[must_use]
    pub fn exclude(
        mut self,
        entity: impl Into<String>,
        owner: impl Into<String>,
        reason: ExclusionReason,
    ) -> Self {
        self.excluded
            .push(ExcludedEntity::new(entity, owner, reason));
        self
    }

    /// The blob cross-check that ran against the journal's referenced hashes.
    #[must_use]
    pub fn blob_cross_check(mut self, cross_check: BlobCrossCheck) -> Self {
        self.blobs = Some(cross_check);
        self
    }

    /// Builds the report as of `now_millis` (unix milliseconds, supplied by the caller).
    #[must_use]
    pub fn build(self, now_millis: i64) -> RecoveryReport {
        let mut reasons = Vec::new();

        if self.tenant.trim().is_empty() {
            reasons.push(Blocker::TenantMissing);
        }
        if self.target.trim().is_empty() {
            reasons.push(Blocker::JournalTargetMissing);
        }
        if self.region.trim().is_empty() {
            reasons.push(Blocker::RegionMissing);
        }
        if self.policy.is_none() {
            reasons.push(Blocker::DurabilityPolicyUnknown);
        }
        match (self.verified, self.verified_within_millis) {
            (Some(verified), Some(within)) if within > 0 => {
                if verified.at_millis > now_millis {
                    reasons.push(Blocker::VerifiedAtInTheFuture);
                } else {
                    let age = now_millis.saturating_sub(verified.at_millis);
                    if age > within {
                        reasons.push(Blocker::VerificationStale {
                            age_millis: age,
                            limit_millis: within,
                        });
                    }
                }
            }
            (Some(_), _) => reasons.push(Blocker::VerificationWindowInvalid),
            (None, _) => reasons.push(Blocker::NeverVerified),
        }
        if self.snapshot.is_none() {
            reasons.push(Blocker::NoSnapshot);
        }
        match self.key_backup {
            KeyBackupStatus::Confirmed { .. } => {}
            KeyBackupStatus::Never => reasons.push(Blocker::KeyBackupNever),
            KeyBackupStatus::Failed => reasons.push(Blocker::KeyBackupFailed),
            KeyBackupStatus::Unverified => reasons.push(Blocker::KeyBackupUnverified),
        }

        let mut excluded = self.excluded;
        excluded.sort_by(|a, b| a.entity.cmp(&b.entity));
        excluded.dedup_by(|a, b| a.entity == b.entity);
        for excluded_entity in &excluded {
            if excluded_entity.entity.trim().is_empty() {
                reasons.push(Blocker::ExcludedEntityNameless);
            } else if excluded_entity.owner.trim().is_empty() {
                reasons.push(Blocker::ExcludedWithoutOwner {
                    entity: excluded_entity.entity.clone(),
                });
            }
        }

        match &self.blobs {
            None => reasons.push(Blocker::BlobCrossCheckNotRun),
            Some(cross_check) => {
                if !cross_check.is_clean() {
                    reasons.push(Blocker::BlobsMissing {
                        count: cross_check.missing.len() as u64,
                    });
                }
            }
        }

        let could_recover = if reasons.is_empty() {
            Verdict::Yes
        } else {
            Verdict::No { reasons }
        };

        RecoveryReport {
            tenant: self.tenant,
            journal: JournalTarget {
                target: self.target,
                region: self.region,
            },
            durability_policy: self.policy,
            last_verified: self.verified,
            oldest_snapshot: self.snapshot,
            key_backup: self.key_backup,
            excluded_entities: excluded,
            blob_cross_check: self.blobs,
            could_recover,
        }
    }
}
