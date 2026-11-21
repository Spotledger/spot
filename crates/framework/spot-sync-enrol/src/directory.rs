//! [`DurableDirectory`]: the `NodeDirectory` the hub authenticates against, backed by the
//! enrolment store.
//!
//! `resolve` is synchronous and runs on the request path, so it **never touches the database**:
//! it reads an in-memory snapshot that the host refreshes ([`DurableDirectory::refresh`], at
//! least every [`DirectoryConfig::max_staleness`] seconds, from a background job). If refreshes
//! keep failing, the snapshot ages, and beyond [`DirectoryConfig::fail_closed_after`] seconds the
//! directory **authenticates nobody** (an unreadable directory is not "allow"). A revocation or
//! suspension made through [`DurableDirectory::mark`] (the admin path calls it right after the
//! store commits) takes effect in the snapshot at once, without waiting for the next refresh.
//!
//! The directory is per tenant (its store is), and maps a certificate fingerprint to the node,
//! the partitions the node owns and the tenant's own lifecycle state: the same certificate pinned
//! in two tenants authenticates only where it is enrolled.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use spot_sync_http::{AuthenticatedNode, ClientCertificate, Error, NodeDirectory};
use spot_sync_protocol::NodeId;

use crate::cert::SKEW_SECONDS;
use crate::error::Result;
use crate::model::{CertState, NodeStatus, PinnedCert};
use crate::store::EnrolmentStore;

/// The clock the directory reads time through (Unix seconds); time is never read ambiently.
pub trait Clock: Send + Sync {
    /// Now, in Unix seconds.
    fn now(&self) -> i64;
}

/// The directory's freshness bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryConfig {
    /// The longest a healthy snapshot is allowed to be, seconds (the host refreshes at least this
    /// often).
    pub max_staleness: i64,
    /// Beyond this age the directory refuses everyone, seconds.
    pub fail_closed_after: i64,
}

impl Default for DirectoryConfig {
    fn default() -> Self {
        Self {
            max_staleness: 5,
            fail_closed_after: 30,
        }
    }
}

#[derive(Default)]
struct Snapshot {
    certs: HashMap<String, PinnedCert>,
    refreshed_at: Option<i64>,
}

/// The durable node directory of one tenant.
pub struct DurableDirectory {
    store: Arc<dyn EnrolmentStore>,
    clock: Arc<dyn Clock>,
    config: DirectoryConfig,
    snapshot: RwLock<Snapshot>,
}

impl std::fmt::Debug for DurableDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableDirectory")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl DurableDirectory {
    /// A directory over `store`. It starts **empty and never refreshed**, so it authenticates
    /// nobody until the first successful [`refresh`](Self::refresh).
    #[must_use]
    pub fn new(
        store: Arc<dyn EnrolmentStore>,
        clock: Arc<dyn Clock>,
        config: DirectoryConfig,
    ) -> Self {
        Self {
            store,
            clock,
            config,
            snapshot: RwLock::new(Snapshot::default()),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Snapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Snapshot> {
        self.snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reloads the snapshot from the store. On a failure the old snapshot is kept (and keeps
    /// ageing), and the failure is returned for the host to log.
    ///
    /// # Errors
    /// The store's failure.
    pub fn refresh(&self) -> Result<()> {
        let now = self.clock.now();
        let rows = self.store.snapshot(now)?;
        let certs = rows
            .into_iter()
            .map(|row| (row.fingerprint.clone(), row))
            .collect();
        let mut snapshot = self.write();
        snapshot.certs = certs;
        snapshot.refreshed_at = Some(now);
        Ok(())
    }

    /// Whether the snapshot is older than [`DirectoryConfig::max_staleness`] (or was never
    /// loaded): the host's refresh job uses it.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        match self.read().refreshed_at {
            Some(at) => self.clock.now() - at > self.config.max_staleness,
            None => true,
        }
    }

    /// Applies a status change to the local snapshot at once (the admin path calls it right
    /// after the store committed), so a revoked or suspended node is refused on its next request
    /// without waiting for a refresh.
    pub fn mark(&self, node: &NodeId, status: NodeStatus) {
        let mut snapshot = self.write();
        for cert in snapshot.certs.values_mut().filter(|c| c.node == *node) {
            cert.node_status = status;
            if status == NodeStatus::Revoked {
                cert.cert_state = CertState::Revoked;
            }
        }
    }

    /// Whether the snapshot is recent enough to trust (the directory authenticates only then).
    /// [`enrolled`](Self::enrolled) is empty when this is `false`, so a caller that must tell "no
    /// nodes" from "cannot tell" asks this first.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        let now = self.clock.now();
        self.read()
            .refreshed_at
            .is_some_and(|at| now - at <= self.config.fail_closed_after)
    }

    /// The nodes the snapshot knows, with their status, ordered by node id: one entry per node
    /// however many certificates it holds (a rotation overlap, a retired certificate).
    ///
    /// It reads only the in-memory snapshot the host refreshes, never the store, so it is cheap
    /// and never blocks on the database. A node revoked through this process shows as revoked at
    /// once ([`mark`](Self::mark)); one revoked elsewhere shows after the next
    /// [`refresh`](Self::refresh), which the host runs at least every
    /// [`DirectoryConfig::max_staleness`] seconds. A node that has a pending enrolment code but
    /// has not redeemed it has no certificate and is **not listed**. When the snapshot is too old
    /// to trust (the directory then authenticates nobody) the list is empty: an unreadable
    /// directory lists no one rather than a stale roster.
    ///
    /// A node whose certificates disagree on the status (which one store write cannot produce)
    /// is listed under the most severe one: revoked over suspended over active.
    #[must_use]
    pub fn enrolled(&self) -> Vec<(NodeId, NodeStatus)> {
        if !self.is_fresh() {
            return Vec::new();
        }
        let snapshot = self.read();
        let mut by_node: std::collections::BTreeMap<String, (NodeId, NodeStatus)> =
            std::collections::BTreeMap::new();
        for cert in snapshot.certs.values() {
            let status = if cert.cert_state == CertState::Revoked {
                NodeStatus::Revoked
            } else {
                cert.node_status
            };
            by_node
                .entry(cert.node.to_string())
                .and_modify(|(_, seen)| {
                    if severity(status) > severity(*seen) {
                        *seen = status;
                    }
                })
                .or_insert_with(|| (cert.node.clone(), status));
        }
        by_node.into_values().collect()
    }

    /// Adds or replaces one certificate in the local snapshot (after an enrolment or a rotation
    /// committed), so the new certificate authenticates at once.
    pub fn learn(&self, cert: PinnedCert) {
        self.write().certs.insert(cert.fingerprint.clone(), cert);
    }
}

/// Ranks a status for [`DurableDirectory::enrolled`]: the most severe wins.
const fn severity(status: NodeStatus) -> u8 {
    match status {
        NodeStatus::Active => 0,
        NodeStatus::Suspended => 1,
        NodeStatus::Revoked => 2,
    }
}

impl NodeDirectory for DurableDirectory {
    fn resolve(
        &self,
        certificate: &ClientCertificate,
    ) -> spot_sync_http::Result<AuthenticatedNode> {
        let now = self.clock.now();
        let snapshot = self.read();
        let fresh = snapshot
            .refreshed_at
            .is_some_and(|at| now - at <= self.config.fail_closed_after);
        if !fresh {
            return Err(Error::identity_unknown(
                "the node directory is too old to trust, so no node is authenticated",
            ));
        }
        let Some(pinned) = snapshot.certs.get(certificate.fingerprint()) else {
            return Err(Error::identity_unknown(
                "the certificate is not enrolled for this tenant",
            ));
        };
        if pinned.cert_state == CertState::Revoked || pinned.node_status == NodeStatus::Revoked {
            return Err(Error::identity_revoked(format!(
                "the certificate for `{}` was revoked",
                pinned.node
            )));
        }
        if pinned.node_status == NodeStatus::Suspended {
            return Err(Error::identity_revoked(format!(
                "the node `{}` is suspended",
                pinned.node
            )));
        }
        if pinned.cert_state == CertState::Retired {
            return Err(Error::identity_unknown("the certificate is retired"));
        }
        if now + SKEW_SECONDS < pinned.not_before || now - SKEW_SECONDS > pinned.not_after {
            return Err(Error::identity_unknown(
                "the certificate is outside its validity",
            ));
        }
        Ok(AuthenticatedNode::new(
            pinned.node.clone(),
            pinned.partitions.iter().cloned(),
        ))
    }
}
