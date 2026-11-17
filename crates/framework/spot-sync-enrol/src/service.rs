//! [`EnrolService`]: the administrator's and the node's operations, over a store and a directory.
//!
//! The service holds no state of its own. It issues codes from an injected [`RandomSource`],
//! numbers audit records from injected [`Ids`], reads time through an injected [`Clock`], applies
//! every change to the store (which commits it **with** its audit record) and then to the
//! directory's local snapshot, so a revocation or a suspension is effective at once.

use std::sync::Arc;

use spot_sync_protocol::NodeId;

use crate::cert::{ParsedCert, SKEW_SECONDS};
use crate::code::{self, RandomSource};
use crate::directory::{Clock, DurableDirectory};
use crate::error::{EnrolError, Result};
use crate::model::{
    AuditEvent, CertState, DEFAULT_CODE_TTL_SECONDS, ExpiringCert, NewEnrolment, NodeStatus,
    NodeSummary, PinnedCert, RedeemRequest, Redeemed, Rotation,
};
use crate::node::ProofOfPossession;
use crate::store::EnrolmentStore;

/// Fresh identifiers for audit records and enrolments; the caller's UUID source in production.
pub trait Ids: Send + Sync {
    /// A new UUID as text (lower-case, hyphenated).
    fn uuid(&self) -> String;
    /// A new enrolment id: 1 to 64 of `[a-z0-9_]`.
    fn enrolment_id(&self) -> String;
}

/// A code just issued: shown to the administrator **once** and never stored.
pub struct IssuedEnrolment {
    /// The enrolment id (public; it is in the audit record).
    pub id: String,
    /// The code text to hand to the operator of the node.
    pub code: String,
}

impl std::fmt::Debug for IssuedEnrolment {
    /// Never shows the code.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedEnrolment")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What an administrator asks for when creating an enrolment.
#[derive(Clone, Debug)]
pub struct EnrolRequest {
    /// The node to enrol.
    pub node: NodeId,
    /// Its role.
    pub role: String,
    /// The partitions it will own.
    pub partitions: Vec<String>,
    /// The subscription template applied on redemption (opaque here).
    pub template: String,
    /// How long the code lives, seconds; `None` is 24 hours.
    pub ttl_seconds: Option<i64>,
}

/// The enrolment service of one tenant.
pub struct EnrolService {
    store: Arc<dyn EnrolmentStore>,
    directory: Arc<DurableDirectory>,
    random: Arc<dyn RandomSource>,
    ids: Arc<dyn Ids>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for EnrolService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolService").finish_non_exhaustive()
    }
}

impl EnrolService {
    /// A service over the given parts.
    #[must_use]
    pub fn new(
        store: Arc<dyn EnrolmentStore>,
        directory: Arc<DurableDirectory>,
        random: Arc<dyn RandomSource>,
        ids: Arc<dyn Ids>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            directory,
            random,
            ids,
            clock,
        }
    }

    fn audit(
        &self,
        action: &'static str,
        node: &NodeId,
        actor: &str,
        enrolment_id: Option<String>,
        fingerprint: Option<String>,
        at: i64,
    ) -> AuditEvent {
        AuditEvent {
            action,
            node: node.clone(),
            enrolment_id,
            fingerprint,
            actor: actor.to_owned(),
            at,
            transaction_id: self.ids.uuid(),
            event_id: self.ids.uuid(),
            subject_id: self.ids.uuid(),
        }
    }

    /// Creates an enrolment for `request.node` and returns its one-time code.
    ///
    /// # Errors
    /// `sync_enrol.node_exists`; `sync_enrol.invalid_argument` for a malformed role, partition or
    /// lifetime; the store's failure.
    pub fn create_enrolment(&self, admin: &str, request: &EnrolRequest) -> Result<IssuedEnrolment> {
        let now = self.clock.now();
        let ttl = request.ttl_seconds.unwrap_or(DEFAULT_CODE_TTL_SECONDS);
        let role_ok = !request.role.is_empty()
            && request.role.len() <= 64
            && request
                .role
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !role_ok
            || ttl <= 0
            || request.partitions.is_empty()
            || request.partitions.len() > 256
            || request
                .partitions
                .iter()
                .any(|p| p.is_empty() || p.len() > 128 || p.chars().any(char::is_control))
            || request.template.len() > 64 * 1024
            || admin.is_empty()
        {
            return Err(EnrolError::InvalidArgument {
                reason: "the role, partitions, template, lifetime or administrator is malformed",
            });
        }
        let issued = code::issue(&self.ids.enrolment_id(), self.random.as_ref())?;
        let audit = self.audit(
            "enrolled",
            &request.node,
            admin,
            Some(issued.id.clone()),
            None,
            now,
        );
        let new = NewEnrolment {
            id: issued.id.clone(),
            node: request.node.clone(),
            role: request.role.clone(),
            partitions: request.partitions.clone(),
            template: request.template.clone(),
            salt: issued.salt,
            hash: issued.hash,
            created_by: admin.to_owned(),
            created_at: now,
            expires_at: now.saturating_add(ttl),
        };
        self.store.create_enrolment(&new, &audit)?;
        Ok(IssuedEnrolment {
            id: issued.id.clone(),
            code: issued.text.clone(),
        })
    }

    /// Redeems a presented code for `node` with its self-signed certificate. A malformed code is
    /// counted and answered like any other wrong code.
    ///
    /// # Errors
    /// As [`EnrolmentStore::redeem`], and `sync_enrol.cert_invalid` for an unacceptable
    /// certificate.
    pub fn redeem(
        &self,
        presented_code: &str,
        node: &NodeId,
        cert_der: &[u8],
        source: &str,
    ) -> Result<Redeemed> {
        let now = self.clock.now();
        let cert = ParsedCert::parse(cert_der, now)?;
        let (id, secret) = match code::parse(presented_code) {
            Ok(parsed) => (parsed.id, parsed.secret),
            // A malformed code goes through the store too, so it counts against the source.
            Err(_) => (String::new(), vec![0u8; code::SECRET_BYTES]),
        };
        let audit = self.audit(
            "redeemed",
            node,
            node.as_str(),
            (!id.is_empty()).then(|| id.clone()),
            Some(cert.fingerprint().to_owned()),
            now,
        );
        let request = RedeemRequest {
            id: &id,
            secret: &secret,
            node,
            cert: &cert,
            source,
            now,
        };
        let redeemed = self.store.redeem(&request, &audit)?;
        self.directory.learn(PinnedCert {
            fingerprint: cert.fingerprint().to_owned(),
            node: node.clone(),
            partitions: redeemed.partitions.iter().cloned().collect(),
            node_status: NodeStatus::Active,
            cert_state: CertState::Active,
            not_before: cert.not_before(),
            not_after: cert.not_after(),
        });
        Ok(redeemed)
    }

    /// Rotates `node` to `new_cert_der`, proven by a signature of the **current** key made at
    /// `proof_at` (within five minutes of now).
    ///
    /// # Errors
    /// `sync_enrol.proof_invalid`, `sync_enrol.cert_invalid`, and the store's refusals.
    pub fn rotate(
        &self,
        node: &NodeId,
        new_cert_der: &[u8],
        proof: &ProofOfPossession,
        proof_at: i64,
        overlap_seconds: i64,
    ) -> Result<()> {
        let now = self.clock.now();
        if (now - proof_at).abs() > SKEW_SECONDS {
            return Err(EnrolError::ProofInvalid);
        }
        let new_cert = ParsedCert::parse(new_cert_der, now)?;
        let current = self.store.active_certificate(node)?;
        proof.verify(&current, node, new_cert.fingerprint(), proof_at)?;
        let audit = self.audit(
            "rotated",
            node,
            node.as_str(),
            None,
            Some(new_cert.fingerprint().to_owned()),
            now,
        );
        self.store.rotate(
            &Rotation {
                node,
                new_cert: &new_cert,
                overlap_seconds,
                now,
            },
            &audit,
        )?;
        // The store is the truth; refresh the local snapshot (best effort: it also ages out).
        let _ = self.directory.refresh();
        Ok(())
    }

    /// Suspends a node: its next request is refused.
    ///
    /// # Errors
    /// `sync_enrol.node_unknown`, `sync_enrol.node_revoked`, the store's failure.
    pub fn suspend(&self, admin: &str, node: &NodeId) -> Result<()> {
        self.set(admin, node, NodeStatus::Suspended, "suspended")
    }

    /// Resumes a suspended node.
    ///
    /// # Errors
    /// As [`suspend`](Self::suspend).
    pub fn resume(&self, admin: &str, node: &NodeId) -> Result<()> {
        self.set(admin, node, NodeStatus::Active, "resumed")
    }

    /// Revokes a node and every certificate of it, for good. Effective in the local snapshot at
    /// once.
    ///
    /// # Errors
    /// As [`suspend`](Self::suspend).
    pub fn revoke(&self, admin: &str, node: &NodeId) -> Result<()> {
        self.set(admin, node, NodeStatus::Revoked, "revoked")
    }

    fn set(&self, admin: &str, node: &NodeId, to: NodeStatus, action: &'static str) -> Result<()> {
        let now = self.clock.now();
        let audit = self.audit(action, node, admin, None, None, now);
        self.store.set_status(node, to, now, &audit)?;
        self.directory.mark(node, to);
        Ok(())
    }

    /// Every node of the tenant.
    ///
    /// # Errors
    /// The store's failure.
    pub fn list(&self) -> Result<Vec<NodeSummary>> {
        self.store.list()
    }

    /// The active certificates close to their end, for the hub service's `CERT_EXPIRING`
    /// condition (amber inside 30 days, red inside 3).
    ///
    /// # Errors
    /// The store's failure.
    pub fn expiring(&self) -> Result<Vec<ExpiringCert>> {
        self.store.expiring(self.clock.now())
    }
}
