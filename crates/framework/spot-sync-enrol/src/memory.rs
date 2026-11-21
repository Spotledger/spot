//! [`MemoryEnrolmentStore`]: the reference semantics of the store contract, in one process.
//!
//! Every operation runs under one lock and either completes with its audit record or changes
//! nothing, so the in-memory store is atomic the same way the PostgreSQL one is. It is for tests
//! and single-process development: it forgets everything on restart.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use spot_sync_protocol::NodeId;

use crate::code::verify_secret;
use crate::error::{EnrolError, Result};
use crate::model::{
    AuditEvent, CertState, ExpiringCert, LOCKOUT_SECONDS, MAX_FAILURES, NewEnrolment, NodeStatus,
    NodeSummary, PinnedCert, RedeemRequest, Redeemed, Rotation, clamp_overlap, effective_state,
    severity,
};
use crate::store::EnrolmentStore;

struct EnrolmentRow {
    node: String,
    role: String,
    partitions: Vec<String>,
    template: String,
    salt: [u8; 16],
    hash: [u8; 32],
    expires_at: i64,
    used_at: Option<i64>,
}

struct NodeRow {
    role: String,
    status: NodeStatus,
    partitions: Vec<String>,
}

struct CertRow {
    node: String,
    state: CertState,
    der: Vec<u8>,
    not_before: i64,
    not_after: i64,
    retire_at: Option<i64>,
}

#[derive(Default)]
struct Attempt {
    failures: u32,
    window_start: i64,
    locked_until: i64,
}

#[derive(Default)]
struct Inner {
    enrolments: BTreeMap<String, EnrolmentRow>,
    nodes: BTreeMap<String, NodeRow>,
    certs: BTreeMap<String, CertRow>,
    attempts: BTreeMap<String, Attempt>,
    audit: Vec<AuditEvent>,
    fail_audit: bool,
}

/// The in-memory enrolment store.
#[derive(Default)]
pub struct MemoryEnrolmentStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryEnrolmentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryEnrolmentStore")
            .finish_non_exhaustive()
    }
}

impl MemoryEnrolmentStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The audit records committed so far, oldest first.
    #[must_use]
    pub fn audit_log(&self) -> Vec<AuditEvent> {
        self.lock().audit.clone()
    }

    /// Makes every later write fail as if the audit record could not be stored: the change must
    /// not happen either (the atomicity test).
    pub fn fail_audit_writes(&self, fail: bool) {
        self.lock().fail_audit = fail;
    }
}

fn audit_gate(inner: &Inner) -> Result<()> {
    if inner.fail_audit {
        Err(EnrolError::StoreFailed {
            reason: "the audit record could not be written, so nothing was changed",
        })
    } else {
        Ok(())
    }
}

fn node_id(text: &str) -> Result<NodeId> {
    NodeId::new(text).map_err(|_| EnrolError::StoreFailed {
        reason: "a stored node id could not be read",
    })
}

fn record_failure(inner: &mut Inner, source: &str, now: i64) {
    let attempt = inner.attempts.entry(source.to_owned()).or_default();
    if now - attempt.window_start >= LOCKOUT_SECONDS {
        attempt.failures = 0;
        attempt.window_start = now;
    }
    attempt.failures += 1;
    if attempt.failures >= MAX_FAILURES {
        attempt.locked_until = now + LOCKOUT_SECONDS;
        attempt.failures = 0;
        attempt.window_start = now;
    }
}

impl EnrolmentStore for MemoryEnrolmentStore {
    fn create_enrolment(&self, new: &NewEnrolment, audit: &AuditEvent) -> Result<()> {
        let mut inner = self.lock();
        audit_gate(&inner)?;
        if inner
            .nodes
            .get(new.node.as_str())
            .is_some_and(|n| n.status != NodeStatus::Revoked)
        {
            return Err(EnrolError::NodeExists);
        }
        if inner.enrolments.contains_key(&new.id) {
            return Err(EnrolError::InvalidArgument {
                reason: "the enrolment id is already used",
            });
        }
        inner.enrolments.insert(
            new.id.clone(),
            EnrolmentRow {
                node: new.node.as_str().to_owned(),
                role: new.role.clone(),
                partitions: new.partitions.clone(),
                template: new.template.clone(),
                salt: new.salt,
                hash: new.hash,
                expires_at: new.expires_at,
                used_at: None,
            },
        );
        inner.audit.push(audit.clone());
        Ok(())
    }

    fn redeem(&self, request: &RedeemRequest<'_>, audit: &AuditEvent) -> Result<Redeemed> {
        let mut inner = self.lock();
        audit_gate(&inner)?;
        let now = request.now;
        if inner
            .attempts
            .get(request.source)
            .is_some_and(|a| a.locked_until > now)
        {
            return Err(EnrolError::LockedOut);
        }
        // The secret is always checked, against a dummy salt and hash for an unknown id, so the
        // work (and the answer) is the same whichever way the code is wrong.
        let (salt, hash) = inner
            .enrolments
            .get(request.id)
            .map_or(([0u8; 16], [0u8; 32]), |e| (e.salt, e.hash));
        let secret_ok = verify_secret(&salt, &hash, request.secret);
        let usable = inner.enrolments.get(request.id).is_some_and(|e| {
            e.used_at.is_none() && e.expires_at > now && e.node == request.node.as_str()
        });
        if !(secret_ok && usable) {
            record_failure(&mut inner, request.source, now);
            return Err(EnrolError::CodeInvalid);
        }
        // The code is good. Refusals from here do not consume it.
        if inner
            .nodes
            .get(request.node.as_str())
            .is_some_and(|n| n.status != NodeStatus::Revoked)
        {
            return Err(EnrolError::NodeExists);
        }
        if let Some(existing) = inner.certs.get(request.cert.fingerprint()) {
            return Err(if existing.state == CertState::Revoked {
                EnrolError::CertRevoked
            } else {
                EnrolError::CertInvalid
            });
        }
        let Some(enrolment) = inner.enrolments.get_mut(request.id) else {
            return Err(EnrolError::CodeInvalid);
        };
        enrolment.used_at = Some(now);
        let redeemed = Redeemed {
            node: request.node.clone(),
            partitions: enrolment.partitions.clone(),
            role: enrolment.role.clone(),
            template: enrolment.template.clone(),
            fingerprint: request.cert.fingerprint().to_owned(),
        };
        let (role, partitions) = (enrolment.role.clone(), enrolment.partitions.clone());
        inner.nodes.insert(
            request.node.as_str().to_owned(),
            NodeRow {
                role,
                status: NodeStatus::Active,
                partitions,
            },
        );
        inner.certs.insert(
            request.cert.fingerprint().to_owned(),
            CertRow {
                node: request.node.as_str().to_owned(),
                state: CertState::Active,
                der: request.cert.der().to_vec(),
                not_before: request.cert.not_before(),
                not_after: request.cert.not_after(),
                retire_at: None,
            },
        );
        inner.audit.push(audit.clone());
        Ok(redeemed)
    }

    fn rotate(&self, rotation: &Rotation<'_>, audit: &AuditEvent) -> Result<()> {
        let mut inner = self.lock();
        audit_gate(&inner)?;
        let node = rotation.node.as_str();
        match inner.nodes.get(node) {
            None => return Err(EnrolError::NodeUnknown),
            Some(row) if row.status == NodeStatus::Revoked => return Err(EnrolError::NodeRevoked),
            Some(_) => {}
        }
        if let Some(existing) = inner.certs.get(rotation.new_cert.fingerprint()) {
            return Err(if existing.state == CertState::Revoked {
                EnrolError::CertRevoked
            } else {
                EnrolError::CertInvalid
            });
        }
        let end = rotation.now + clamp_overlap(rotation.overlap_seconds);
        for row in inner.certs.values_mut().filter(|c| c.node == node) {
            match row.state {
                CertState::Overlap => {
                    row.state = CertState::Retired;
                    row.retire_at = Some(rotation.now);
                }
                CertState::Active => {
                    row.state = CertState::Overlap;
                    row.retire_at = Some(end);
                }
                CertState::Retired | CertState::Revoked => {}
            }
        }
        inner.certs.insert(
            rotation.new_cert.fingerprint().to_owned(),
            CertRow {
                node: node.to_owned(),
                state: CertState::Active,
                der: rotation.new_cert.der().to_vec(),
                not_before: rotation.new_cert.not_before(),
                not_after: rotation.new_cert.not_after(),
                retire_at: None,
            },
        );
        inner.audit.push(audit.clone());
        Ok(())
    }

    fn set_status(
        &self,
        node: &NodeId,
        to: NodeStatus,
        _now: i64,
        audit: &AuditEvent,
    ) -> Result<()> {
        let mut inner = self.lock();
        audit_gate(&inner)?;
        match inner.nodes.get(node.as_str()) {
            None => return Err(EnrolError::NodeUnknown),
            Some(row) if row.status == NodeStatus::Revoked => return Err(EnrolError::NodeRevoked),
            Some(_) => {}
        }
        if let Some(row) = inner.nodes.get_mut(node.as_str()) {
            row.status = to;
        }
        if to == NodeStatus::Revoked {
            for cert in inner.certs.values_mut().filter(|c| c.node == node.as_str()) {
                cert.state = CertState::Revoked;
                cert.retire_at = None;
            }
        }
        inner.audit.push(audit.clone());
        Ok(())
    }

    fn list(&self) -> Result<Vec<NodeSummary>> {
        let inner = self.lock();
        inner
            .nodes
            .iter()
            .map(|(name, row)| {
                let active = inner
                    .certs
                    .iter()
                    .find(|(_, c)| c.node == *name && c.state == CertState::Active);
                Ok(NodeSummary {
                    node: node_id(name)?,
                    role: row.role.clone(),
                    status: row.status,
                    partitions: row.partitions.clone(),
                    fingerprint: active.map(|(fp, _)| fp.clone()),
                    not_after: active.map(|(_, c)| c.not_after),
                })
            })
            .collect()
    }

    fn snapshot(&self, now: i64) -> Result<Vec<PinnedCert>> {
        let inner = self.lock();
        let mut out = Vec::new();
        for (fingerprint, cert) in &inner.certs {
            let state = effective_state(cert.state, cert.retire_at, now);
            if state == CertState::Retired {
                continue;
            }
            let Some(node) = inner.nodes.get(&cert.node) else {
                continue;
            };
            out.push(PinnedCert {
                fingerprint: fingerprint.clone(),
                node: node_id(&cert.node)?,
                partitions: node.partitions.iter().cloned().collect::<BTreeSet<_>>(),
                node_status: node.status,
                cert_state: state,
                not_before: cert.not_before,
                not_after: cert.not_after,
            });
        }
        Ok(out)
    }

    fn active_certificate(&self, node: &NodeId) -> Result<Vec<u8>> {
        let inner = self.lock();
        inner
            .certs
            .values()
            .find(|c| c.node == node.as_str() && c.state == CertState::Active)
            .map(|c| c.der.clone())
            .ok_or(EnrolError::NodeUnknown)
    }

    fn expiring(&self, now: i64) -> Result<Vec<ExpiringCert>> {
        let inner = self.lock();
        let mut out = Vec::new();
        for (fingerprint, cert) in &inner.certs {
            let active_node = inner
                .nodes
                .get(&cert.node)
                .is_some_and(|n| n.status == NodeStatus::Active);
            if cert.state != CertState::Active || !active_node {
                continue;
            }
            if let Some(severity) = severity(cert.not_after, now) {
                out.push(ExpiringCert {
                    node: node_id(&cert.node)?,
                    fingerprint: fingerprint.clone(),
                    not_after: cert.not_after,
                    severity,
                });
            }
        }
        out.sort_by(|a, b| {
            a.not_after
                .cmp(&b.not_after)
                .then(a.fingerprint.cmp(&b.fingerprint))
        });
        Ok(out)
    }
}
