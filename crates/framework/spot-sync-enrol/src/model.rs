//! Plain data: what the stores keep and return.

use std::collections::BTreeSet;

use spot_sync_protocol::NodeId;

use crate::cert::ParsedCert;

/// The default life of an enrolment code: 24 hours.
pub const DEFAULT_CODE_TTL_SECONDS: i64 = 24 * 60 * 60;
/// The default overlap of an old and a new certificate: 7 days.
pub const DEFAULT_OVERLAP_SECONDS: i64 = 7 * 24 * 60 * 60;
/// The hard cap of the overlap: 30 days.
pub const MAX_OVERLAP_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Failed redemptions per source per window before a lockout.
pub const MAX_FAILURES: u32 = 5;
/// The failure-counting window, and the lockout length: one hour.
pub const LOCKOUT_SECONDS: i64 = 60 * 60;
/// A certificate this close to `not_after` raises the amber condition (30 days).
pub const EXPIRY_AMBER_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Inside this the condition is red (3 days).
pub const EXPIRY_RED_SECONDS: i64 = 3 * 24 * 60 * 60;

/// What an administrator creates: one single-use enrolment.
#[derive(Clone, Debug)]
pub struct NewEnrolment {
    /// The public id inside the code.
    pub id: String,
    /// The node the enrolment is for.
    pub node: NodeId,
    /// The node's role (a short lower-case name).
    pub role: String,
    /// The partitions the node will own.
    pub partitions: Vec<String>,
    /// The subscription template the hub applies on redemption (opaque to this crate).
    pub template: String,
    /// The per-code salt.
    pub salt: [u8; 16],
    /// The salted hash of the secret.
    pub hash: [u8; 32],
    /// Who created it.
    pub created_by: String,
    /// When, Unix seconds.
    pub created_at: i64,
    /// When it stops being valid, Unix seconds.
    pub expires_at: i64,
}

/// A redemption attempt.
pub struct RedeemRequest<'a> {
    /// The enrolment id from the code.
    pub id: &'a str,
    /// The secret from the code.
    pub secret: &'a [u8],
    /// The node the presenter says it is.
    pub node: &'a NodeId,
    /// The certificate the node presents (already parsed and checked).
    pub cert: &'a ParsedCert,
    /// Where the attempt came from (an address or a listener name), for the lockout.
    pub source: &'a str,
    /// Now, Unix seconds.
    pub now: i64,
}

impl std::fmt::Debug for RedeemRequest<'_> {
    /// Never shows the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedeemRequest")
            .field("id", &self.id)
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// What a successful redemption returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redeemed {
    /// The node, now active.
    pub node: NodeId,
    /// The partitions it owns.
    pub partitions: Vec<String>,
    /// Its role.
    pub role: String,
    /// The subscription template of the enrolment.
    pub template: String,
    /// The pinned fingerprint.
    pub fingerprint: String,
}

/// A certificate rotation.
pub struct Rotation<'a> {
    /// The node.
    pub node: &'a NodeId,
    /// The new certificate (already parsed and checked).
    pub new_cert: &'a ParsedCert,
    /// How long the old certificate stays valid beside the new one, seconds.
    pub overlap_seconds: i64,
    /// Now, Unix seconds.
    pub now: i64,
}

/// A node's lifecycle status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeStatus {
    /// Authenticates.
    Active,
    /// Refused until resumed.
    Suspended,
    /// Refused for ever.
    Revoked,
}

impl NodeStatus {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Revoked => "revoked",
        }
    }

    /// The status named exactly `text`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// A pinned certificate's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CertState {
    /// The node's current certificate.
    Active,
    /// An earlier certificate, still valid until its overlap ends.
    Overlap,
    /// An earlier certificate whose overlap ended.
    Retired,
    /// Revoked: a tombstone, never pinned again.
    Revoked,
}

impl CertState {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Overlap => "overlap",
            Self::Retired => "retired",
            Self::Revoked => "revoked",
        }
    }

    /// The state named exactly `text`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "overlap" => Some(Self::Overlap),
            "retired" => Some(Self::Retired),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One pinned certificate with its node, as the directory reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedCert {
    /// The SHA-256 fingerprint.
    pub fingerprint: String,
    /// The node it identifies.
    pub node: NodeId,
    /// The partitions the node owns.
    pub partitions: BTreeSet<String>,
    /// The node's status.
    pub node_status: NodeStatus,
    /// The certificate's state **at the instant of the snapshot** (an overlap whose end has
    /// passed reads as retired).
    pub cert_state: CertState,
    /// Start of validity, Unix seconds.
    pub not_before: i64,
    /// End of validity, Unix seconds.
    pub not_after: i64,
}

/// A node as an administrator lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSummary {
    /// The node.
    pub node: NodeId,
    /// Its role.
    pub role: String,
    /// Its status.
    pub status: NodeStatus,
    /// The partitions it owns.
    pub partitions: Vec<String>,
    /// The fingerprint of its active certificate.
    pub fingerprint: Option<String>,
    /// When the active certificate expires, Unix seconds.
    pub not_after: Option<i64>,
}

/// How urgent a certificate expiry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Within 30 days of `not_after`.
    Amber,
    /// Within 3 days of `not_after`, or already past it.
    Red,
}

/// An active certificate close to its end, for the hub service's `CERT_EXPIRING` condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiringCert {
    /// The node.
    pub node: NodeId,
    /// The certificate's fingerprint (public).
    pub fingerprint: String,
    /// When it expires, Unix seconds.
    pub not_after: i64,
    /// Amber or red.
    pub severity: Severity,
}

/// The audit record that must commit with every change (a `sys_sync_node_admin` outbox event).
/// It names the action, the node, the enrolment id and the certificate fingerprint (public) and
/// **never** a code, a hash, a salt or a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// `enrolled`, `redeemed`, `rotated`, `suspended`, `resumed`, `revoked`.
    pub action: &'static str,
    /// The node.
    pub node: NodeId,
    /// The enrolment id, for the enrolment actions.
    pub enrolment_id: Option<String>,
    /// The certificate fingerprint, for the certificate actions.
    pub fingerprint: Option<String>,
    /// Who acted (an administrator id, or the node itself).
    pub actor: String,
    /// When, Unix seconds.
    pub at: i64,
    /// The outbox transaction id (a UUID text) the record is written under.
    pub transaction_id: String,
    /// The outbox event id (a UUID text).
    pub event_id: String,
    /// The subject record id (a UUID text) of the node-admin event.
    pub subject_id: String,
}

/// The audit topic every record is written to.
pub const AUDIT_TOPIC: &str = "sys_sync_node_admin";

/// The effective state of a stored certificate at `now`: an overlap whose end has passed is
/// retired.
#[must_use]
pub fn effective_state(stored: CertState, retire_at: Option<i64>, now: i64) -> CertState {
    match (stored, retire_at) {
        (CertState::Overlap, Some(end)) if now >= end => CertState::Retired,
        (state, _) => state,
    }
}

/// The overlap to use: the request clamped to `1..=MAX_OVERLAP_SECONDS`.
#[must_use]
pub fn clamp_overlap(seconds: i64) -> i64 {
    seconds.clamp(1, MAX_OVERLAP_SECONDS)
}

/// The severity of a certificate ending at `not_after` at `now`, if it needs attention.
#[must_use]
pub fn severity(not_after: i64, now: i64) -> Option<Severity> {
    let left = not_after - now;
    if left <= EXPIRY_RED_SECONDS {
        Some(Severity::Red)
    } else if left <= EXPIRY_AMBER_SECONDS {
        Some(Severity::Amber)
    } else {
        None
    }
}
