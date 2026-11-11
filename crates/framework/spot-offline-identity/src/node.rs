//! Node identity (ADR-070 rule 12.13 h): the short-lived certificate, suspension, and the
//! gate in front of key release.
//!
//! - [`CertLifecycle`] models the node's certificate. It is renewed only while connected
//!   ([`CertLifecycle::renew`] refuses otherwise) and **lapses on its own** on the monotonic
//!   clock if the node stays offline. A reboot cannot extend it (same rule as the lease).
//! - [`NodeRegistry`] is the hub's view of its nodes. Suspending or decommissioning a node
//!   makes [`NodeRegistry::revocation_entries`] list it, and the next snapshot carries that
//!   entry to the other nodes.
//! - [`GatedKeyRelease`] sits in front of the key-release service of `spot-protected-keys`
//!   (behind the [`KeyReleasePort`] trait): a suspended, decommissioned, unknown or
//!   impersonated node gets no key, and every refusal is audited.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::audit::{ACTOR_NODE, AuditSink, actor, audit_record, text};
use crate::error::OfflineError;
use crate::lease::{Lease, LeaseRecord};
use crate::snapshot::{NodeRevocationKind, Revocation};
use crate::store::{Persistence, SnapshotStore};
use crate::types::{NodeClock, NodeId, TenantId, Tier};

/// The longest a node certificate may live: 24 hours.
pub const MAX_CERT_TTL_MS: u64 = 24 * 3_600_000;

/// The persisted part of a certificate's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertRecord {
    /// The certificate serial.
    pub serial: u64,
    /// The remaining life, in the first slot of the lease record.
    pub lease: LeaseRecord,
}

/// The node's certificate lifecycle. The certificate itself belongs to the TLS layer; this is
/// the model of how long it is good for.
#[derive(Clone, Copy, Debug)]
pub struct CertLifecycle {
    serial: u64,
    life: Lease,
}

impl CertLifecycle {
    /// No certificate yet.
    #[must_use]
    pub fn none(now: NodeClock) -> Self {
        Self {
            serial: 0,
            life: Lease::lapsed(now),
        }
    }

    pub(crate) fn restore(record: Option<&CertRecord>, now: NodeClock) -> Self {
        match record {
            Some(r) => Self {
                serial: r.serial,
                life: Lease::restore(&r.lease, now, 0),
            },
            None => Self::none(now),
        }
    }

    pub(crate) fn record(&self, now: NodeClock) -> Option<CertRecord> {
        (self.serial != 0).then(|| CertRecord {
            serial: self.serial,
            lease: self.life.record(now),
        })
    }

    /// Takes a freshly issued certificate of `serial` and `ttl_ms`.
    ///
    /// # Errors
    /// [`OfflineError::CertificateOffline`] when the node is not connected: a certificate is
    /// only ever renewed through the hub. [`OfflineError::CertificateLifetime`] for a zero or
    /// over-long lifetime. [`OfflineError::CertificateSerial`] when the serial does not
    /// advance (an older certificate replayed).
    pub fn renew(
        &mut self,
        serial: u64,
        ttl_ms: u64,
        connected: bool,
        now: NodeClock,
    ) -> Result<(), OfflineError> {
        if !connected {
            return Err(OfflineError::CertificateOffline);
        }
        if ttl_ms == 0 || ttl_ms > MAX_CERT_TTL_MS {
            return Err(OfflineError::CertificateLifetime);
        }
        if serial <= self.serial {
            return Err(OfflineError::CertificateSerial);
        }
        self.serial = serial;
        self.life = Lease::start([ttl_ms, 0, 0], now);
        Ok(())
    }

    /// Milliseconds of life left at `now`; zero once lapsed or on a clock fault.
    #[must_use]
    pub fn remaining_ms(&self, now: NodeClock) -> u64 {
        self.life.remaining(Tier::One, now)
    }

    /// Whether the certificate is still good at `now`.
    #[must_use]
    pub fn is_valid(&self, now: NodeClock) -> bool {
        self.serial != 0 && self.remaining_ms(now) > 0
    }
}

impl<P: Persistence> SnapshotStore<P> {
    /// Takes a renewed node certificate (see [`CertLifecycle::renew`]). It is persisted with
    /// the next accept or checkpoint; losing it to a crash only shortens it.
    ///
    /// # Errors
    /// As [`CertLifecycle::renew`].
    pub fn renew_certificate(
        &mut self,
        serial: u64,
        ttl_ms: u64,
        connected: bool,
        now: NodeClock,
    ) -> Result<(), OfflineError> {
        self.cert.renew(serial, ttl_ms, connected, now)
    }

    /// Whether the node's certificate is good at `now`.
    #[must_use]
    pub fn certificate_valid(&self, now: NodeClock) -> bool {
        self.cert.is_valid(now)
    }
}

/// A node's state on the hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Taking work.
    Active,
    /// Connections and key release refused; it may be reinstated.
    Suspended,
    /// Retired for good.
    Decommissioned,
}

/// The hub's registry of one tenant's nodes.
#[derive(Clone, Debug)]
pub struct NodeRegistry {
    tenant: TenantId,
    nodes: BTreeMap<NodeId, (NodeState, u64)>,
}

impl NodeRegistry {
    /// An empty registry for `tenant`.
    #[must_use]
    pub fn new(tenant: TenantId) -> Self {
        Self {
            tenant,
            nodes: BTreeMap::new(),
        }
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// Registers a node as active. A node that is already known keeps its state: a suspended
    /// node cannot be re-activated by registering it again.
    pub fn register(&mut self, node: NodeId) {
        self.nodes.entry(node).or_insert((NodeState::Active, 0));
    }

    /// The state of `node`, or `None` if it is not registered.
    #[must_use]
    pub fn state(&self, node: &NodeId) -> Option<NodeState> {
        self.nodes.get(node).map(|(s, _)| *s)
    }

    /// Suspends `node`, effective from snapshot epoch `epoch`.
    ///
    /// # Errors
    /// [`OfflineError::UnknownNode`] for a node that is not registered.
    pub fn suspend(&mut self, node: &NodeId, epoch: u64) -> Result<(), OfflineError> {
        self.set(node, NodeState::Suspended, epoch)
    }

    /// Decommissions `node`, effective from snapshot epoch `epoch`.
    ///
    /// # Errors
    /// [`OfflineError::UnknownNode`] for a node that is not registered.
    pub fn decommission(&mut self, node: &NodeId, epoch: u64) -> Result<(), OfflineError> {
        self.set(node, NodeState::Decommissioned, epoch)
    }

    /// Reinstates a suspended node. A decommissioned node is final and stays so.
    ///
    /// # Errors
    /// [`OfflineError::UnknownNode`] for a node that is not registered or is decommissioned.
    pub fn reinstate(&mut self, node: &NodeId) -> Result<(), OfflineError> {
        match self.nodes.get_mut(node) {
            Some((state @ NodeState::Suspended, epoch)) => {
                *state = NodeState::Active;
                *epoch = 0;
                Ok(())
            }
            Some((NodeState::Active, _)) => Ok(()),
            _ => Err(OfflineError::UnknownNode),
        }
    }

    fn set(&mut self, node: &NodeId, state: NodeState, epoch: u64) -> Result<(), OfflineError> {
        match self.nodes.get_mut(node) {
            Some((NodeState::Decommissioned, _)) => Ok(()),
            Some(entry) => {
                *entry = (state, epoch);
                Ok(())
            }
            None => Err(OfflineError::UnknownNode),
        }
    }

    /// The revocation entries the next snapshot of every node carries.
    #[must_use]
    pub fn revocation_entries(&self) -> Vec<Revocation> {
        self.nodes
            .iter()
            .filter_map(|(node, (state, epoch))| {
                let kind = match state {
                    NodeState::Active => return None,
                    NodeState::Suspended => NodeRevocationKind::Suspended,
                    NodeState::Decommissioned => NodeRevocationKind::Decommissioned,
                };
                Some(Revocation::Node {
                    node: node.as_str().to_owned(),
                    kind,
                    effective_epoch: *epoch,
                })
            })
            .collect()
    }
}

/// A request for a group's data key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyReleaseRequest {
    /// The tenant the request is for.
    pub tenant: TenantId,
    /// The node the payload says it is.
    pub claimed_node: NodeId,
    /// The node the connection authenticated as (mutual TLS). Only this one is trusted.
    pub authenticated_node: NodeId,
    /// The protection group.
    pub group: String,
    /// The audit timestamp (microseconds); never used for any lease.
    pub at_micros: i64,
}

/// The key-release service of `spot-protected-keys`, as this crate sees it: it returns the
/// group key already wrapped to the node (ciphertext only). The adapter that calls
/// `Hub::release` implements this trait.
pub trait KeyReleasePort {
    /// Releases the wrapped key.
    ///
    /// # Errors
    /// [`OfflineError`] when the release is refused or fails.
    fn release(&mut self, request: &KeyReleaseRequest) -> Result<Vec<u8>, OfflineError>;
}

/// Gates a [`KeyReleasePort`] on the node's state. Fails closed: only a registered, active
/// node of the right tenant, authenticated as the node it claims to be, reaches the inner port.
pub struct GatedKeyRelease<'a, P: KeyReleasePort, A: AuditSink> {
    registry: &'a NodeRegistry,
    inner: P,
    audit: A,
}

impl<'a, P: KeyReleasePort, A: AuditSink> GatedKeyRelease<'a, P, A> {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(registry: &'a NodeRegistry, inner: P, audit: A) -> Self {
        Self {
            registry,
            inner,
            audit,
        }
    }

    /// The audit sink (tests read it).
    #[must_use]
    pub fn audit(&self) -> &A {
        &self.audit
    }

    fn refuse(&mut self, request: &KeyReleaseRequest, reason: &'static str) -> OfflineError {
        let record = audit_record(
            "offline_identity.key_release_refused",
            &actor(ACTOR_NODE, request.authenticated_node.as_str()),
            request.at_micros,
            &[
                ("node", text(request.claimed_node.as_str())),
                ("group", text(&request.group)),
                ("reason", text(reason)),
            ],
        );
        // A failing audit never turns a refusal into a release.
        let _ = self.audit.record(record);
        OfflineError::KeyReleaseRefused {
            node: request.claimed_node.as_str().to_owned(),
            reason,
        }
    }
}

impl<P: KeyReleasePort, A: AuditSink> KeyReleasePort for GatedKeyRelease<'_, P, A> {
    fn release(&mut self, request: &KeyReleaseRequest) -> Result<Vec<u8>, OfflineError> {
        if &request.tenant != self.registry.tenant() {
            return Err(self.refuse(request, "another tenant"));
        }
        if request.claimed_node != request.authenticated_node {
            return Err(self.refuse(request, "the node is not the one that authenticated"));
        }
        match self.registry.state(&request.authenticated_node) {
            Some(NodeState::Active) => {}
            Some(NodeState::Suspended) => return Err(self.refuse(request, "the node is suspended")),
            Some(NodeState::Decommissioned) => {
                return Err(self.refuse(request, "the node is decommissioned"));
            }
            None => return Err(self.refuse(request, "the node is not registered")),
        }
        // Audit first: if the record cannot be written, no key leaves.
        let record = audit_record(
            concat!("offline_identity", ".key_release_allowed"),
            &actor(ACTOR_NODE, request.authenticated_node.as_str()),
            request.at_micros,
            &[("group", text(&request.group))],
        );
        if self.audit.record(record).is_err() {
            return Err(OfflineError::AuditFailed);
        }
        self.inner.release(request)
    }
}
