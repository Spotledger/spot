//! The store seam: everything an enrolment changes goes through one of these methods, and each
//! change commits **together with its audit record** (an enrolment, a revocation or a rotation
//! without its `sys_sync_node_admin` event cannot commit).
//!
//! Two implementations run the same contract suite: [`crate::MemoryEnrolmentStore`] and
//! [`crate::PgEnrolmentStore`]. Every method is scoped to the store's tenant.

use spot_sync_protocol::NodeId;

use crate::error::Result;
use crate::model::{
    AuditEvent, ExpiringCert, NewEnrolment, NodeStatus, NodeSummary, PinnedCert, RedeemRequest,
    Redeemed, Rotation,
};

/// The enrolment store of one tenant.
pub trait EnrolmentStore: Send + Sync {
    /// Creates a single-use enrolment.
    ///
    /// # Errors
    /// `sync_enrol.node_exists` when the node already has an identity (it must be revoked first,
    /// and a revoked node is never re-enrolled under the same id); `sync_enrol.invalid_argument`
    /// for a duplicate enrolment id; the store's failure (nothing changed, audit included).
    fn create_enrolment(&self, new: &NewEnrolment, audit: &AuditEvent) -> Result<()>;

    /// Redeems a code: verifies it (constant time), consumes it (one winner among concurrent
    /// redemptions), creates the node `active` and pins the presented certificate, all in one
    /// step with the audit record. The failures "unknown", "expired", "used" and "does not fit
    /// the node" are one error, and every failure counts against `source` (5 per hour, then a
    /// 1 hour lockout).
    ///
    /// # Errors
    /// `sync_enrol.locked_out`; `sync_enrol.code_invalid`; `sync_enrol.node_exists`;
    /// `sync_enrol.cert_revoked` for a revoked fingerprint; `sync_enrol.cert_invalid` for a
    /// fingerprint already pinned to another node; the store's failure.
    fn redeem(&self, request: &RedeemRequest<'_>, audit: &AuditEvent) -> Result<Redeemed>;

    /// Pins a new certificate for an active node: the new one becomes `active`, the old one
    /// `overlap` until `now + overlap`, any older overlap is retired now.
    ///
    /// # Errors
    /// `sync_enrol.node_unknown`; `sync_enrol.node_revoked`; `sync_enrol.cert_revoked`;
    /// `sync_enrol.cert_invalid` for a fingerprint already pinned; the store's failure.
    fn rotate(&self, rotation: &Rotation<'_>, audit: &AuditEvent) -> Result<()>;

    /// Suspends, resumes or revokes a node. Revocation also revokes every certificate of the node
    /// (tombstones) and is final; suspension and resumption never touch a revoked node.
    ///
    /// # Errors
    /// `sync_enrol.node_unknown`; `sync_enrol.node_revoked`; the store's failure.
    fn set_status(&self, node: &NodeId, to: NodeStatus, now: i64, audit: &AuditEvent)
    -> Result<()>;

    /// Every node of the tenant, ordered by id.
    ///
    /// # Errors
    /// The store's failure.
    fn list(&self) -> Result<Vec<NodeSummary>>;

    /// Every certificate row that can still matter at `now`, with its effective state: what the
    /// directory loads. Retired certificates are omitted; revoked ones are included (they are how
    /// a revoked fingerprint is told from an unknown one).
    ///
    /// # Errors
    /// The store's failure.
    fn snapshot(&self, now: i64) -> Result<Vec<PinnedCert>>;

    /// The DER of the node's active certificate, for the proof of possession of a rotation.
    ///
    /// # Errors
    /// `sync_enrol.node_unknown`; the store's failure.
    fn active_certificate(&self, node: &NodeId) -> Result<Vec<u8>>;

    /// The active certificates of active nodes inside the amber window at `now` (red inside 3
    /// days or past the end), for the `CERT_EXPIRING` condition.
    ///
    /// # Errors
    /// The store's failure.
    fn expiring(&self, now: i64) -> Result<Vec<ExpiringCert>>;
}
