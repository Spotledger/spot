//! The key handles: a group data key's identity and a node's wrapping key.

use crate::bind;
use crate::crypto;
use crate::error::ProtectedKeyError;
use crate::ids::{GroupName, KeyEpoch, NodeId, TenantId};
use crate::locked::LockedKey;
use crate::root::RootKeyProvider;
use crate::store::{RootRef, WrappedNodeKey};
use serde::{Deserialize, Serialize};
use spot_secrets::{KeyRef, SecretBytes, WrappedKey};
use std::fmt;

/// The identity of a group's data key: the group and the epoch it belongs to.
///
/// It is a handle, never the material: the material lives in a [`LockedKey`] held by the hub
/// or by a node's [`crate::node::NodeKeyStore`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GroupDataKey {
    group: GroupName,
    epoch: KeyEpoch,
}

impl GroupDataKey {
    /// The key of `group` at `epoch`.
    #[must_use]
    pub fn new(group: GroupName, epoch: KeyEpoch) -> Self {
        Self { group, epoch }
    }

    /// The protection group.
    #[must_use]
    pub fn group(&self) -> &GroupName {
        &self.group
    }

    /// The epoch.
    #[must_use]
    pub fn epoch(&self) -> KeyEpoch {
        self.epoch
    }
}

impl fmt::Display for GroupDataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.group, self.epoch)
    }
}

/// A node's per-node wrapping key (ADR-070 rule 12.13 e).
///
/// The hub wraps a group data key under it, so the hub can stop releasing keys to one node
/// without touching the others. The material is never exposed; the type only wraps and unwraps.
pub struct NodeWrapKey {
    node: NodeId,
    key: LockedKey,
}

impl NodeWrapKey {
    /// A fresh random wrapping key for `node`.
    pub fn generate(node: NodeId) -> Result<Self, ProtectedKeyError> {
        Ok(Self {
            node,
            key: LockedKey::random()?,
        })
    }

    /// A wrapping key for `node` from caller-supplied material (a test seam).
    #[must_use]
    pub fn from_key(node: NodeId, key: LockedKey) -> Self {
        Self { node, key }
    }

    /// The node it belongs to.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// An explicit copy, for the hub and the node to hold one each. Both are zeroised on drop.
    pub fn duplicate(&self) -> Result<Self, ProtectedKeyError> {
        Ok(Self {
            node: self.node.clone(),
            key: self.key.duplicate(),
        })
    }

    /// The at-rest form of this key: wrapped under the tenant root key by `root`, bound to the
    /// tenant and the node, so a store holds the node key without ever holding it in the clear.
    pub fn wrap_under_root(
        &self,
        tenant: &TenantId,
        root: &dyn RootKeyProvider,
    ) -> Result<WrappedNodeKey, ProtectedKeyError> {
        let aad = bind::node_wrap_ad(tenant, &self.node);
        let wrapped = root.wrap(&self.key, &aad)?;
        Ok(WrappedNodeKey {
            root: RootRef {
                id: wrapped.key.id,
                version: wrapped.key.version,
            },
            bytes: wrapped.bytes,
        })
    }

    /// Rebuilds the wrapping key of `node` from its at-rest form.
    pub fn unwrap_from_root(
        node: &NodeId,
        tenant: &TenantId,
        wrapped: &WrappedNodeKey,
        root: &dyn RootKeyProvider,
    ) -> Result<Self, ProtectedKeyError> {
        let aad = bind::node_wrap_ad(tenant, node);
        let stored = WrappedKey {
            key: KeyRef {
                id: wrapped.root.id.clone(),
                version: wrapped.root.version,
            },
            bytes: wrapped.bytes.clone(),
        };
        let plain = root.unwrap(&stored, &aad)?;
        Ok(Self {
            node: node.clone(),
            key: LockedKey::from_bytes(plain.expose())?,
        })
    }

    /// Asks `lock` to pin the wrapping key's memory.
    pub fn pin(&self, lock: &dyn crate::locked::MemoryLock) -> bool {
        self.key.pin(lock)
    }

    /// Wraps `plaintext` under the node key, binding `aad`.
    pub(crate) fn wrap(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, ProtectedKeyError> {
        crypto::seal(self.key.expose(), aad, plaintext)
    }

    /// Unwraps `wrapped` under the node key, checking `aad`.
    pub(crate) fn unwrap(
        &self,
        aad: &[u8],
        wrapped: &[u8],
    ) -> Result<SecretBytes, ProtectedKeyError> {
        crypto::open(self.key.expose(), aad, wrapped)
    }
}

impl fmt::Debug for NodeWrapKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeWrapKey")
            .field("node", &self.node)
            .field("key", &"[redacted]")
            .finish()
    }
}
