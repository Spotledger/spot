//! [`NodeKeyStore`]: the node's lease-bound memory of released group keys.
//!
//! A node accepts a [`KeyRelease`] only if it is addressed to it, its lease has not passed and
//! it is not revoked; it unwraps the group key under its own wrapping key and keeps it in
//! locked memory. The key is discarded the moment the lease passes (ADR-070 rule 12.13 c and
//! e). Nothing here reads a clock: callers pass a monotonic [`MonoTime`].

use crate::bind;
use crate::error::ProtectedKeyError;
use crate::ids::{GroupName, KeyEpoch, NodeId};
use crate::keys::NodeWrapKey;
use crate::locked::{LockedKey, MemoryLock, NoLock};
use crate::release::KeyRelease;
use crate::time::MonoTime;
use std::collections::BTreeMap;

/// A key the node holds, and the lease it holds it under.
struct Held {
    key: LockedKey,
    lease_until: MonoTime,
}

/// The node side of the hierarchy: the wrapping key and the keys released under a lease.
pub struct NodeKeyStore {
    node: NodeId,
    wrap_key: NodeWrapKey,
    held: BTreeMap<(GroupName, KeyEpoch), Held>,
    lock: Box<dyn MemoryLock>,
    /// The epoch at or after which this node is revoked, once it learns of it.
    revocation_epoch: Option<KeyEpoch>,
}

impl NodeKeyStore {
    /// A store for `node` with its wrapping key, using the no-op memory lock.
    #[must_use]
    pub fn new(node: NodeId, wrap_key: NodeWrapKey) -> Self {
        Self::with_lock(node, wrap_key, Box::new(NoLock))
    }

    /// A store with a caller-supplied [`MemoryLock`] (a host that can pin memory).
    #[must_use]
    pub fn with_lock(node: NodeId, wrap_key: NodeWrapKey, lock: Box<dyn MemoryLock>) -> Self {
        Self {
            node,
            wrap_key,
            held: BTreeMap::new(),
            lock,
            revocation_epoch: None,
        }
    }

    /// The node.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The memory lock in use.
    #[must_use]
    pub fn lock(&self) -> &dyn MemoryLock {
        self.lock.as_ref()
    }

    /// The epoch at or after which this node is revoked, once recorded.
    #[must_use]
    pub fn revocation_epoch(&self) -> Option<KeyEpoch> {
        self.revocation_epoch
    }

    /// Accepts `release`: checks it is addressed here, its lease is live, the node is not
    /// revoked for the epoch, unwraps the group key and keeps it in locked memory.
    pub fn accept(&mut self, release: &KeyRelease, now: MonoTime) -> Result<(), ProtectedKeyError> {
        if release.node != self.node {
            return Err(ProtectedKeyError::WrongNode {
                node: self.node.clone(),
                addressed: release.node.clone(),
            });
        }
        if let Some(revoked) = self.revocation_epoch
            && release.epoch >= revoked
        {
            return Err(ProtectedKeyError::NodeRevoked {
                node: self.node.clone(),
            });
        }
        if release.is_expired(now) {
            return Err(ProtectedKeyError::LeaseExpired {
                group: release.group.clone(),
            });
        }
        let aad = bind::release_ad(
            &release.tenant,
            &release.group,
            release.epoch,
            &release.node,
        );
        let plain = self.wrap_key.unwrap(&aad, &release.wrapped_key)?;
        let key = LockedKey::from_bytes(plain.expose())?;
        let _pinned = key.pin(self.lock.as_ref());
        self.held.insert(
            (release.group.clone(), release.epoch),
            Held {
                key,
                lease_until: release.lease_until,
            },
        );
        Ok(())
    }

    /// Records that this node is revoked at `epoch` and forgets every key it holds. Releases
    /// of an epoch at or after this one are refused.
    pub fn record_revocation(&mut self, epoch: KeyEpoch) {
        self.revocation_epoch = Some(epoch);
        self.forget_all();
    }

    /// Whether the node holds `group` at `epoch`.
    #[must_use]
    pub fn is_held(&self, group: &GroupName, epoch: KeyEpoch) -> bool {
        self.held.contains_key(&(group.clone(), epoch))
    }

    /// The lease end for a held key, if held.
    #[must_use]
    pub fn lease_until(&self, group: &GroupName, epoch: KeyEpoch) -> Option<MonoTime> {
        self.held
            .get(&(group.clone(), epoch))
            .map(|h| h.lease_until)
    }

    /// Every `(group, epoch)` the node holds.
    #[must_use]
    pub fn held(&self) -> Vec<(GroupName, KeyEpoch)> {
        self.held.keys().cloned().collect()
    }

    /// Drops every held key (they are zeroised).
    pub fn forget_all(&mut self) {
        self.held.clear();
    }

    /// Discards every key whose lease has passed at `now`; returns how many were dropped.
    pub fn discard_expired(&mut self, now: MonoTime) -> usize {
        let before = self.held.len();
        self.held
            .retain(|_, held| !now.is_at_or_after(held.lease_until));
        before - self.held.len()
    }

    /// The held key for `(group, epoch)`, after discarding expired leases.
    pub fn key(
        &mut self,
        group: &GroupName,
        epoch: KeyEpoch,
        now: MonoTime,
    ) -> Result<&LockedKey, ProtectedKeyError> {
        self.discard_expired(now);
        self.held
            .get(&(group.clone(), epoch))
            .map(|held| &held.key)
            .ok_or_else(|| ProtectedKeyError::UnknownEpoch {
                group: group.clone(),
                epoch,
            })
    }

    /// Runs `f` with the held key for `(group, epoch)`, after discarding expired leases.
    pub fn with_key<R>(
        &mut self,
        group: &GroupName,
        epoch: KeyEpoch,
        now: MonoTime,
        f: impl FnOnce(&LockedKey) -> R,
    ) -> Result<R, ProtectedKeyError> {
        self.discard_expired(now);
        match self.held.get(&(group.clone(), epoch)) {
            Some(held) => Ok(f(&held.key)),
            None => Err(ProtectedKeyError::UnknownEpoch {
                group: group.clone(),
                epoch,
            }),
        }
    }
}
