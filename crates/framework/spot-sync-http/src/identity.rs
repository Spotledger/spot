//! Node identity: the presented client certificate, the node it maps to, and the directory that
//! decides.
//!
//! ADR-062 section 3: every connection is outbound from the edge, over TLS with node identity.
//! The hub authenticates the edge by its client certificate. This module is the seam: a
//! [`ClientCertificate`] is whatever the TLS layer saw, a [`NodeDirectory`] maps it to an
//! [`AuthenticatedNode`], and the directory is the only place pinning and revocation are decided.
//!
//! The real directory arrives with the control plane (node enrolment, rotation and revocation);
//! [`StaticDirectory`] is the in-process reference used by tests and by a single-node deployment
//! until then. It pins by the certificate's SHA-256 fingerprint, so a presented certificate that
//! is not enrolled is refused rather than trusted.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use spot_sync_protocol::NodeId;

use crate::error::{Error, Result};

/// A client certificate as the TLS layer presents it: the DER bytes and, when they parse, the
/// subject and the fingerprint.
///
/// The fingerprint is what the reference directory pins; the subject is what a directory that
/// keys by name uses. Neither is a secret (both are public certificate material).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCertificate {
    der: Vec<u8>,
    subject: Option<String>,
    fingerprint: String,
}

impl ClientCertificate {
    /// Wraps a certificate's DER bytes; the subject is parsed when the bytes are a valid X.509
    /// certificate, and the fingerprint is always computed.
    pub fn from_der(der: impl Into<Vec<u8>>) -> Self {
        let der = der.into();
        let mut hasher = Sha256::new();
        hasher.update(&der);
        let digest = hasher.finalize();
        let mut fingerprint = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(&mut fingerprint, "{byte:02x}");
        }
        let subject = subject_of(&der);
        Self {
            der,
            subject,
            fingerprint,
        }
    }

    /// The certificate's DER bytes.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The certificate's SHA-256 fingerprint, lower-case hex (the pinning key).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The certificate's subject in RFC 2253 form, when the DER parsed.
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }
}

/// Parses the subject of a DER certificate, returning `None` when it is not readable.
fn subject_of(der: &[u8]) -> Option<String> {
    use x509_parser::prelude::FromDer as _;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(der).ok()?;
    Some(certificate.subject().to_string())
}

/// A node the hub authenticated, and the source partitions it is allowed to write.
///
/// A node owns the partitions of the streams it produces; the hub refuses a batch whose source
/// partition is not in this set, before it decodes any record beyond the batch header
/// (ADR-062 G1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedNode {
    /// The node's identity.
    pub node: NodeId,
    /// The partitions this node owns.
    pub partitions: BTreeSet<String>,
}

impl AuthenticatedNode {
    /// A node that owns the given partitions.
    pub fn new(node: NodeId, partitions: impl IntoIterator<Item = String>) -> Self {
        Self {
            node,
            partitions: partitions.into_iter().collect(),
        }
    }

    /// Whether this node owns `source`'s partition.
    ///
    /// A source with no partition is owned by no node, so it is never accepted on the push path.
    pub fn owns(&self, source: &spot_events_contract::SourceId) -> bool {
        // The site must match too: a partition name is only unique inside its site, so
        // `other-tenant#branch_01` must not be accepted for a node that owns `branch_01`.
        source.site() == self.node.site()
            && source
                .partition()
                .is_some_and(|partition| self.partitions.contains(partition))
    }

    /// Whether this node may pull `source`: a stream it owns, or one of the hub's own site
    /// (`hub_site`). A stream of any other site or of another node's partition is refused.
    pub fn may_pull(&self, source: &spot_events_contract::SourceId, hub_site: &str) -> bool {
        self.owns(source) || source.site() == hub_site
    }
}

/// Maps a presented client certificate to the node it identifies.
///
/// Implementations decide pinning (which certificate is enrolled) and revocation (which is no
/// longer admitted). Both failures are errors, never a fallback to an anonymous identity.
pub trait NodeDirectory: Send + Sync + 'static {
    /// Resolves `certificate` to the node it authenticates.
    ///
    /// # Errors
    ///
    /// [`Error::identity_unknown`] when the certificate is not enrolled, and
    /// [`Error::identity_revoked`] when it was revoked.
    fn resolve(&self, certificate: &ClientCertificate) -> Result<AuthenticatedNode>;
}

/// An entry in a [`StaticDirectory`]: the node and the partitions a pinned certificate owns.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    node: NodeId,
    partitions: BTreeSet<String>,
    revoked: bool,
}

/// An in-process [`NodeDirectory`] keyed by certificate fingerprint.
///
/// It pins each enrolled certificate by its SHA-256 fingerprint and answers revocation from the
/// same table, so a certificate that is not enrolled, or that was revoked, is refused. This is
/// the reference directory for tests and a single-node deployment; the control plane supplies
/// the real one.
#[derive(Clone, Debug, Default)]
pub struct StaticDirectory {
    entries: BTreeMap<String, Entry>,
}

impl StaticDirectory {
    /// An empty directory: every certificate is unknown until it is enrolled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enrols a certificate fingerprint for `node`, which owns `partitions`.
    pub fn enroll(
        &mut self,
        fingerprint: impl Into<String>,
        node: NodeId,
        partitions: impl IntoIterator<Item = String>,
    ) -> &mut Self {
        self.entries.insert(
            fingerprint.into(),
            Entry {
                node,
                partitions: partitions.into_iter().collect(),
                revoked: false,
            },
        );
        self
    }

    /// Marks an enrolled certificate revoked. A fingerprint that is not enrolled is ignored.
    pub fn revoke(&mut self, fingerprint: &str) -> &mut Self {
        if let Some(entry) = self.entries.get_mut(fingerprint) {
            entry.revoked = true;
        }
        self
    }

    /// The number of enrolled certificates.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is enrolled.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl NodeDirectory for StaticDirectory {
    fn resolve(&self, certificate: &ClientCertificate) -> Result<AuthenticatedNode> {
        match self.entries.get(certificate.fingerprint()) {
            Some(entry) if entry.revoked => Err(Error::identity_revoked(format!(
                "the certificate for `{}` was revoked",
                entry.node
            ))),
            Some(entry) => Ok(AuthenticatedNode {
                node: entry.node.clone(),
                partitions: entry.partitions.clone(),
            }),
            None => Err(Error::identity_unknown(format!(
                "no node is enrolled for certificate fingerprint {}",
                certificate.fingerprint()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn certificate(seed: u8) -> ClientCertificate {
        // A syntactically plausible DER blob is enough for fingerprinting; the subject stays
        // `None` because it is not a real certificate.
        ClientCertificate::from_der(vec![0x30, 0x82, seed, seed, seed])
    }

    #[test]
    fn fingerprints_are_stable_and_differ() {
        let a = certificate(1);
        let b = certificate(2);
        assert_eq!(a.fingerprint().len(), 64);
        assert_eq!(a, certificate(1));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert!(a.subject().is_none());
    }

    #[test]
    fn an_unenrolled_certificate_is_unknown() {
        let directory = StaticDirectory::new();
        let error = directory.resolve(&certificate(1)).unwrap_err();
        assert_eq!(error.code, crate::error::IDENTITY_UNKNOWN);
    }

    #[test]
    fn an_enrolled_certificate_maps_to_its_node_and_partitions() {
        let cert = certificate(3);
        let node = NodeId::new("edge-khi#branch_01").unwrap();
        let mut directory = StaticDirectory::new();
        directory.enroll(cert.fingerprint(), node.clone(), ["branch_01".to_owned()]);
        let authenticated = directory.resolve(&cert).unwrap();
        assert_eq!(authenticated.node, node);
        assert!(authenticated.partitions.contains("branch_01"));
    }

    #[test]
    fn a_revoked_certificate_is_refused() {
        let cert = certificate(4);
        let node = NodeId::new("edge-khi").unwrap();
        let mut directory = StaticDirectory::new();
        directory.enroll(cert.fingerprint(), node, ["branch_01".to_owned()]);
        directory.revoke(cert.fingerprint());
        let error = directory.resolve(&cert).unwrap_err();
        assert_eq!(error.code, crate::error::IDENTITY_REVOKED);
    }

    #[test]
    fn ownership_is_by_partition() {
        let node = NodeId::new("edge-khi").unwrap();
        let authenticated = AuthenticatedNode::new(node, ["branch_01".to_owned()]);
        let mine = spot_events_contract::SourceId::new("edge-khi#branch_01").unwrap();
        let theirs = spot_events_contract::SourceId::new("edge-khi#branch_02").unwrap();
        let hub = spot_events_contract::SourceId::new("head-office").unwrap();
        assert!(authenticated.owns(&mine));
        assert!(!authenticated.owns(&theirs));
        assert!(!authenticated.owns(&hub));
    }

    #[test]
    fn another_site_with_the_same_partition_name_is_not_owned_or_pullable() {
        let node = NodeId::new("edge-khi#branch_01").unwrap();
        let authenticated = AuthenticatedNode::new(node, ["branch_01".to_owned()]);
        let foreign = spot_events_contract::SourceId::new("other-tenant#branch_01").unwrap();
        let sibling = spot_events_contract::SourceId::new("edge-khi#branch_02").unwrap();
        let hub = spot_events_contract::SourceId::new("head-office#hub").unwrap();
        assert!(!authenticated.owns(&foreign));
        assert!(!authenticated.may_pull(&foreign, "head-office"));
        assert!(!authenticated.may_pull(&sibling, "head-office"));
        assert!(authenticated.may_pull(&hub, "head-office"));
    }
}
