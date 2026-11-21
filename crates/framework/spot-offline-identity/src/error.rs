//! Typed errors, refusals and reasons with stable `offline_identity.*` codes.
//!
//! Callers match on the code, never on the text. No message carries a key, a signature or a
//! secret: only ids, epochs, tiers and counts. Every code has an entry in
//! `crates/framework/spot-errors/catalogue/offline_identity.toml`.

use crate::types::Tier;

/// Errors that are not a snapshot rejection: bad input, a failing port, a refused operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfflineError {
    /// An id is empty, too long or has control characters.
    #[error("{kind} is not usable ({code})", code = self.code())]
    InvalidId {
        /// Which id.
        kind: &'static str,
    },
    /// A node certificate cannot be renewed because the node has no connection to the hub.
    #[error("the node certificate cannot be renewed while offline ({code})", code = self.code())]
    CertificateOffline,
    /// A certificate lifetime is zero or longer than the platform maximum.
    #[error("the certificate lifetime is outside the allowed range ({code})", code = self.code())]
    CertificateLifetime,
    /// A certificate's serial does not advance, so it could be an older certificate replayed.
    #[error("the certificate serial does not advance ({code})", code = self.code())]
    CertificateSerial,
    /// A journal entry failed its hash-chain check.
    #[error("the offline journal fails its hash chain at entry {seq} ({code})", code = self.code())]
    JournalTampered {
        /// The first entry that fails.
        seq: u64,
    },
    /// A stamp's device sequence does not follow the previous one.
    #[error("the device sequence {got} does not follow {previous} ({code})", code = self.code())]
    JournalSequence {
        /// The last sequence written.
        previous: u64,
        /// The sequence offered.
        got: u64,
    },
    /// The journal store failed to persist an entry; the write must not proceed.
    #[error("the journal store refused the entry ({code})", code = self.code())]
    JournalStore,
    /// An override request was refused.
    #[error("the stale-lease override was refused: {reason} ({code})", code = self.code())]
    OverrideRefused {
        /// Why, in plain words.
        reason: &'static str,
    },
    /// The audit sink failed, so the audited action was not taken.
    #[error("the audit record could not be written, so nothing was done ({code})", code = self.code())]
    AuditFailed,
    /// A key release was refused.
    #[error("key release refused for node {node}: {reason} ({code})", code = self.code())]
    KeyReleaseRefused {
        /// The node.
        node: String,
        /// Why, in plain words.
        reason: &'static str,
    },
    /// A node id is not known to the registry.
    #[error("the node is not registered ({code})", code = self.code())]
    UnknownNode,
    /// A stamp does not decode from its journal record.
    #[error("the journal record is not an offline stamp ({code})", code = self.code())]
    StampMalformed,
    /// The quarantine queue refused or failed to store an item, so the write is not placed.
    #[error("the quarantine queue refused the item ({code})", code = self.code())]
    QuarantineStore,
    /// The quarantine queue already holds a *different* payload for this journal position.
    #[error("the quarantine queue holds another payload for {node} at {journal_seq} ({code})", code = self.code())]
    QuarantineConflict {
        /// The node.
        node: String,
        /// The journal position.
        journal_seq: u64,
    },
}

impl OfflineError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "offline_identity.invalid_id",
            Self::CertificateOffline => "offline_identity.certificate_offline",
            Self::CertificateLifetime => "offline_identity.certificate_lifetime",
            Self::CertificateSerial => "offline_identity.certificate_serial",
            Self::JournalTampered { .. } => "offline_identity.journal_tampered",
            Self::JournalSequence { .. } => "offline_identity.journal_sequence",
            Self::JournalStore => "offline_identity.journal_store",
            Self::OverrideRefused { .. } => "offline_identity.override_refused",
            Self::AuditFailed => "offline_identity.audit_failed",
            Self::KeyReleaseRefused { .. } => "offline_identity.key_release_refused",
            Self::UnknownNode => "offline_identity.unknown_node",
            Self::StampMalformed => "offline_identity.stamp_malformed",
            Self::QuarantineStore => "offline_identity.quarantine_store",
            Self::QuarantineConflict { .. } => "offline_identity.quarantine_conflict",
        }
    }
}

/// Why the node's snapshot store refused a snapshot or timestamp. A rejection changes nothing:
/// the store keeps exactly the state it had.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    /// The epoch is lower than the highest accepted: a rollback.
    #[error("epoch {got} is lower than the accepted epoch {held} ({code})", code = self.code())]
    Rollback {
        /// The epoch offered.
        got: u64,
        /// The highest accepted.
        held: u64,
    },
    /// The epoch equals the accepted epoch but the content differs: a fork.
    #[error("epoch {epoch} is already held with different content ({code})", code = self.code())]
    Fork {
        /// The epoch.
        epoch: u64,
    },
    /// The same snapshot is already held; accepting it again would change nothing.
    #[error("epoch {epoch} is already held ({code})", code = self.code())]
    Replay {
        /// The epoch.
        epoch: u64,
    },
    /// The signature does not verify under the delegated key.
    #[error("the signature does not verify ({code})", code = self.code())]
    BadSignature,
    /// The delegation is not signed by the trusted root, names the wrong tenant, or the signing
    /// key is not the one it delegates.
    #[error("the key delegation is not valid ({code})", code = self.code())]
    BadDelegation,
    /// The delegation version is lower than the highest accepted.
    #[error("the delegation version {got} is lower than {held} ({code})", code = self.code())]
    DelegationRollback {
        /// The version offered.
        got: u64,
        /// The highest accepted.
        held: u64,
    },
    /// The message is for another tenant.
    #[error("the message is for another tenant ({code})", code = self.code())]
    WrongTenant,
    /// The message is for another node.
    #[error("the message is for another node ({code})", code = self.code())]
    WrongNode,
    /// Every tier's `valid_until` has already passed.
    #[error("the message is past its valid_until ({code})", code = self.code())]
    Expired,
    /// A tier's lease is outside the platform floor and ceiling.
    #[error("the {tier} lease is outside the allowed range ({code})", tier = .tier.as_str(), code = self.code())]
    BoundsOutOfRange {
        /// The tier.
        tier: Tier,
    },
    /// The message is not in its documented form.
    #[error("the message is malformed: {detail} ({code})", code = self.code())]
    Malformed {
        /// What is wrong, in plain words.
        detail: &'static str,
    },
    /// A timestamp is not newer than the last accepted message.
    #[error("the timestamp is not newer than the last accepted ({code})", code = self.code())]
    StaleTimestamp,
    /// A timestamp does not belong to the held snapshot (other epoch, other hash, or none held).
    #[error("the timestamp does not belong to the held snapshot ({code})", code = self.code())]
    TimestampMismatch,
    /// The state could not be persisted, so nothing was accepted.
    #[error("the state could not be persisted, so nothing was accepted ({code})", code = self.code())]
    PersistFailed,
}

impl Rejection {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Rollback { .. } => "offline_identity.rollback",
            Self::Fork { .. } => "offline_identity.fork",
            Self::Replay { .. } => "offline_identity.replay",
            Self::BadSignature => "offline_identity.bad_signature",
            Self::BadDelegation => "offline_identity.bad_delegation",
            Self::DelegationRollback { .. } => "offline_identity.delegation_rollback",
            Self::WrongTenant => "offline_identity.wrong_tenant",
            Self::WrongNode => "offline_identity.wrong_node",
            Self::Expired => "offline_identity.expired",
            Self::BoundsOutOfRange { .. } => "offline_identity.bounds_out_of_range",
            Self::Malformed { .. } => "offline_identity.malformed",
            Self::StaleTimestamp => "offline_identity.stale_timestamp",
            Self::TimestampMismatch => "offline_identity.timestamp_mismatch",
            Self::PersistFailed => "offline_identity.persist_failed",
        }
    }
}

/// Why an offline action or sign-in is denied. Every check fails closed: a missing fact is a
/// denial, never an allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// No snapshot has been accepted (or the stored one failed verification).
    NoSnapshot,
    /// This node is suspended or decommissioned in the snapshot it holds.
    NodeSuspended,
    /// The node's certificate has lapsed or was never issued.
    CertificateLapsed,
    /// The monotonic clock went backwards or the boot changed without a restore.
    ClockFault,
    /// The user is not in the directory the node holds.
    UnknownUser,
    /// The directory says the user is disabled.
    UserDisabled,
    /// The user is not on this node's offline allowlist.
    NotOfflineAllowed,
    /// The snapshot revokes the user (or every assignment that would give the right).
    UserRevoked,
    /// The requested tier is more sensitive than the user may act at offline.
    TierNotAllowedForUser,
    /// The lease of this tier has lapsed: the node cannot confirm the user for it.
    TierLapsed(Tier),
    /// No assignment of the user grants the permission.
    PermissionNotGranted,
}

impl DenyReason {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoSnapshot => "offline_identity.deny.no_snapshot",
            Self::NodeSuspended => "offline_identity.deny.node_suspended",
            Self::CertificateLapsed => "offline_identity.deny.certificate_lapsed",
            Self::ClockFault => "offline_identity.deny.clock_fault",
            Self::UnknownUser => "offline_identity.deny.unknown_user",
            Self::UserDisabled => "offline_identity.deny.user_disabled",
            Self::NotOfflineAllowed => "offline_identity.deny.not_offline_allowed",
            Self::UserRevoked => "offline_identity.deny.user_revoked",
            Self::TierNotAllowedForUser => "offline_identity.deny.tier_not_allowed_for_user",
            Self::TierLapsed(_) => "offline_identity.deny.tier_lapsed",
            Self::PermissionNotGranted => "offline_identity.deny.permission_not_granted",
        }
    }
}

/// Why the hub quarantines an offline write. A quarantined write is kept for a person
/// (ADR-067) and never dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineReason {
    /// The stamp names another tenant than the connection's.
    TenantMismatch,
    /// The stamp names another node than the one that authenticated.
    NodeMismatch,
    /// The sending node is suspended or decommissioned.
    SenderSuspended,
    /// The device sequence repeats or goes backwards: a replay.
    SequenceReplay,
    /// The stamp's epoch was never issued to this node.
    UnknownEpoch,
    /// The assignment is not one of the author's.
    AssignmentMismatch,
    /// The author (or the assignment) is revoked.
    AuthorRevoked,
    /// A tier-1 action by a revoked author.
    Tier1AfterRevocation,
    /// The snapshot was older than the tier's bound when the write was made.
    BeyondBound,
    /// The journal's hash chain fails at or before this entry.
    JournalTampered,
}

impl QuarantineReason {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TenantMismatch => "offline_identity.quarantine.tenant_mismatch",
            Self::NodeMismatch => "offline_identity.quarantine.node_mismatch",
            Self::SenderSuspended => "offline_identity.quarantine.sender_suspended",
            Self::SequenceReplay => "offline_identity.quarantine.sequence_replay",
            Self::UnknownEpoch => "offline_identity.quarantine.unknown_epoch",
            Self::AssignmentMismatch => "offline_identity.quarantine.assignment_mismatch",
            Self::AuthorRevoked => "offline_identity.quarantine.author_revoked",
            Self::Tier1AfterRevocation => "offline_identity.quarantine.tier1_after_revocation",
            Self::BeyondBound => "offline_identity.quarantine.beyond_bound",
            Self::JournalTampered => "offline_identity.quarantine.journal_tampered",
        }
    }
}
