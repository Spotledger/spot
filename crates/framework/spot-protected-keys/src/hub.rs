//! [`Hub`]: the head-office side of the key hierarchy.
//!
//! The hub owns the tenant root provider, one data key per protection group and epoch (in
//! locked memory), and one wrapping key per node. It creates and rotates group keys, registers
//! nodes, issues a [`KeyRelease`] under the node's permission tier, and — on revocation —
//! stops release to a node and rotates every group it held (ADR-070 rule 12.13 e).

use crate::bind;
use crate::error::ProtectedKeyError;
use crate::ids::{GroupName, KeyEpoch, NodeId, TenantId};
use crate::keys::{GroupDataKey, NodeWrapKey};
use crate::locked::LockedKey;
use crate::reencrypt::{ReencryptBatch, ReencryptPlan};
use crate::release::KeyRelease;
use crate::root::{RootKeyProvider, root_aad};
use crate::store::{KeyStoreBackup, RootRef, WrappedGroupKey};
use crate::tier::PermissionTier;
use crate::time::MonoTime;
use serde::{Deserialize, Serialize};
use spot_secrets::{KeyRef, WrappedKey};
use std::collections::{BTreeMap, BTreeSet};

/// A node's lifecycle state (ADR-070 rule 12.13 h).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    /// Registered and taking work.
    Active,
    /// Suspended: connections and key release are refused, but the node may return.
    Suspended,
    /// Revoked: release is refused and the groups it held are rotated.
    Revoked,
}

impl NodeStatus {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Revoked => "revoked",
        }
    }
}

/// What a revocation rotated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotatedGroup {
    /// The protection group.
    pub group: GroupName,
    /// The epoch in force when the node was revoked.
    pub from: KeyEpoch,
    /// The epoch created by the rotation.
    pub to: KeyEpoch,
}

/// The outcome of [`Hub::revoke_node`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationReport {
    /// The revoked node.
    pub node: NodeId,
    /// When, on the hub's monotonic clock.
    pub revoked_at: MonoTime,
    /// Every group the node held, rotated to a new epoch.
    pub rotated: Vec<RotatedGroup>,
}

/// One protection group's keys, all epochs, in locked memory.
struct GroupKeys {
    current: KeyEpoch,
    keys: BTreeMap<KeyEpoch, LockedKey>,
    wrapped: BTreeMap<KeyEpoch, WrappedKey>,
    retired: BTreeSet<KeyEpoch>,
    rotated_at: Option<MonoTime>,
}

impl GroupKeys {
    fn empty() -> Self {
        Self {
            current: KeyEpoch::FIRST,
            keys: BTreeMap::new(),
            wrapped: BTreeMap::new(),
            retired: BTreeSet::new(),
            rotated_at: None,
        }
    }

    /// Generates a fresh data key at `epoch`, wraps it under the root and keeps both.
    fn add_epoch<R: RootKeyProvider>(
        &mut self,
        tenant: &TenantId,
        group: &GroupName,
        root: &R,
        epoch: KeyEpoch,
    ) -> Result<(), ProtectedKeyError> {
        let key = LockedKey::random()?;
        let wrapped = root.wrap(&key, &root_aad(tenant, group, epoch))?;
        self.keys.insert(epoch, key);
        self.wrapped.insert(epoch, wrapped);
        self.retired.remove(&epoch);
        self.current = epoch;
        Ok(())
    }
}

/// A node the hub knows about.
struct NodeEntry {
    status: NodeStatus,
    /// The wrapping key; `None` after revocation (the wrapping path is erased).
    wrap_key: Option<NodeWrapKey>,
    /// The last epoch released to the node per group.
    held: BTreeMap<GroupName, KeyEpoch>,
    /// The revocation record, once revoked.
    revocation: Option<Revocation>,
}

/// The epoch in force per group when a node was revoked.
struct Revocation {
    at: MonoTime,
    epochs: BTreeMap<GroupName, KeyEpoch>,
}

/// The hub: group keys, node wrapping keys and the release policy.
pub struct Hub<R: RootKeyProvider> {
    tenant: TenantId,
    root: R,
    groups: BTreeMap<GroupName, GroupKeys>,
    nodes: BTreeMap<NodeId, NodeEntry>,
}

impl<R: RootKeyProvider> Hub<R> {
    /// A hub for the provider's tenant, with no groups and no nodes.
    pub fn new(root: R) -> Self {
        Self {
            tenant: root.tenant().clone(),
            root,
            groups: BTreeMap::new(),
            nodes: BTreeMap::new(),
        }
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The root provider.
    #[must_use]
    pub fn root(&self) -> &R {
        &self.root
    }

    /// Creates `group` at epoch [`KeyEpoch::FIRST`] with a fresh random data key.
    pub fn create_group(
        &mut self,
        group: &GroupName,
        now: MonoTime,
    ) -> Result<KeyEpoch, ProtectedKeyError> {
        if self.groups.contains_key(group) {
            return Err(ProtectedKeyError::GroupExists {
                group: group.clone(),
            });
        }
        let mut keys = GroupKeys::empty();
        keys.add_epoch(&self.tenant, group, &self.root, KeyEpoch::FIRST)?;
        keys.rotated_at = Some(now);
        self.groups.insert(group.clone(), keys);
        tracing::debug!(group = %group, epoch = %KeyEpoch::FIRST, "created protection group");
        Ok(KeyEpoch::FIRST)
    }

    /// Rotates `group` to epoch n+1. Data written afterwards uses the new epoch; older epochs
    /// stay readable.
    pub fn rotate_group(
        &mut self,
        group: &GroupName,
        now: MonoTime,
    ) -> Result<KeyEpoch, ProtectedKeyError> {
        let tenant = self.tenant.clone();
        let keys = self
            .groups
            .get_mut(group)
            .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            })?;
        let next = keys
            .current
            .checked_next()
            .ok_or(ProtectedKeyError::Malformed("epoch overflow"))?;
        keys.add_epoch(&tenant, group, &self.root, next)?;
        keys.rotated_at = Some(now);
        tracing::debug!(group = %group, epoch = %next, "rotated protection group");
        Ok(next)
    }

    /// The current epoch of `group`.
    #[must_use]
    pub fn group_epoch(&self, group: &GroupName) -> Option<KeyEpoch> {
        self.groups.get(group).map(|k| k.current)
    }

    /// Every group name, in order.
    #[must_use]
    pub fn groups(&self) -> Vec<GroupName> {
        self.groups.keys().cloned().collect()
    }

    /// When `group` last rotated, if it ever did.
    #[must_use]
    pub fn rotated_at(&self, group: &GroupName) -> Option<MonoTime> {
        self.groups.get(group).and_then(|k| k.rotated_at)
    }

    /// Retires an epoch's material once its data has been re-encrypted. The current epoch
    /// cannot be retired.
    pub fn retire_epoch(
        &mut self,
        group: &GroupName,
        epoch: KeyEpoch,
    ) -> Result<(), ProtectedKeyError> {
        let keys = self
            .groups
            .get_mut(group)
            .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            })?;
        if epoch == keys.current {
            return Err(ProtectedKeyError::Malformed(
                "cannot retire the current epoch",
            ));
        }
        if keys.keys.remove(&epoch).is_none() {
            return Err(ProtectedKeyError::UnknownEpoch {
                group: group.clone(),
                epoch,
            });
        }
        keys.wrapped.remove(&epoch);
        keys.retired.insert(epoch);
        Ok(())
    }

    /// The handle of a group's current data key.
    pub fn current_key(&self, group: &GroupName) -> Result<GroupDataKey, ProtectedKeyError> {
        let epoch = self
            .group_epoch(group)
            .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            })?;
        Ok(GroupDataKey::new(group.clone(), epoch))
    }

    /// Registers `node` with its wrapping key. A revoked node cannot be re-registered.
    pub fn register_node(
        &mut self,
        node: &NodeId,
        wrap_key: NodeWrapKey,
    ) -> Result<(), ProtectedKeyError> {
        if wrap_key.node() != node {
            return Err(ProtectedKeyError::WrongNode {
                node: node.clone(),
                addressed: wrap_key.node().clone(),
            });
        }
        if let Some(existing) = self.nodes.get(node)
            && existing.status == NodeStatus::Revoked
        {
            return Err(ProtectedKeyError::NodeRevoked { node: node.clone() });
        }
        // Re-registering (a key change) must not forget which groups the node already held,
        // or a later revocation would skip the rotation of those groups.
        let held = self
            .nodes
            .get(node)
            .map(|existing| existing.held.clone())
            .unwrap_or_default();
        self.nodes.insert(
            node.clone(),
            NodeEntry {
                status: NodeStatus::Active,
                wrap_key: Some(wrap_key),
                held,
                revocation: None,
            },
        );
        Ok(())
    }

    /// Suspends or reactivates a node. A revoked node stays revoked.
    pub fn set_status(
        &mut self,
        node: &NodeId,
        status: NodeStatus,
    ) -> Result<(), ProtectedKeyError> {
        let entry = self
            .nodes
            .get_mut(node)
            .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?;
        if entry.status == NodeStatus::Revoked {
            return Err(ProtectedKeyError::NodeRevoked { node: node.clone() });
        }
        entry.status = status;
        Ok(())
    }

    /// The node's status.
    #[must_use]
    pub fn status(&self, node: &NodeId) -> Option<NodeStatus> {
        self.nodes.get(node).map(|e| e.status)
    }

    /// When `node` was revoked, if it was, on the hub's monotonic clock.
    #[must_use]
    pub fn revoked_at(&self, node: &NodeId) -> Option<MonoTime> {
        self.nodes.get(node)?.revocation.as_ref().map(|r| r.at)
    }

    /// The epoch in force for `group` when `node` was revoked, if it was.
    #[must_use]
    pub fn revocation_epoch(&self, node: &NodeId, group: &GroupName) -> Option<KeyEpoch> {
        self.nodes
            .get(node)?
            .revocation
            .as_ref()?
            .epochs
            .get(group)
            .copied()
    }

    /// The epoch last released to `node` for `group`, if any.
    #[must_use]
    pub fn held_epoch(&self, node: &NodeId, group: &GroupName) -> Option<KeyEpoch> {
        self.nodes.get(node)?.held.get(group).copied()
    }

    /// Releases `group`'s current key to `node`, wrapped to its node key, under the tier's
    /// lease. Checks the node is active, has a tier for the group and is not revoked.
    pub fn release(
        &mut self,
        node: &NodeId,
        group: &GroupName,
        now: MonoTime,
        tiers: &dyn PermissionTier,
    ) -> Result<KeyRelease, ProtectedKeyError> {
        let epoch = self
            .group_epoch(group)
            .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            })?;
        self.release_epoch(node, group, epoch, now, tiers)
    }

    /// Releases a specific epoch, for reading old data. Refused for a revoked node when the
    /// epoch is at or after its revocation epoch.
    pub fn release_epoch(
        &mut self,
        node: &NodeId,
        group: &GroupName,
        epoch: KeyEpoch,
        now: MonoTime,
        tiers: &dyn PermissionTier,
    ) -> Result<KeyRelease, ProtectedKeyError> {
        let status = self
            .nodes
            .get(node)
            .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?
            .status;
        match status {
            NodeStatus::Active => {}
            NodeStatus::Suspended => {
                return Err(ProtectedKeyError::NodeNotActive {
                    node: node.clone(),
                    status: "suspended",
                });
            }
            NodeStatus::Revoked => {
                return Err(ProtectedKeyError::NodeRevoked { node: node.clone() });
            }
        }
        if let Some(revoked) = self.revocation_epoch(node, group)
            && epoch >= revoked
        {
            return Err(ProtectedKeyError::NodeRevoked { node: node.clone() });
        }
        let tier = tiers
            .tier(node, group)
            .ok_or_else(|| ProtectedKeyError::NoPermission {
                node: node.clone(),
                group: group.clone(),
            })?;

        let Self {
            tenant,
            groups,
            nodes,
            ..
        } = self;
        let keys = groups
            .get(group)
            .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            })?;
        if keys.retired.contains(&epoch) {
            return Err(ProtectedKeyError::EpochRetired {
                group: group.clone(),
                epoch,
            });
        }
        let group_key = keys
            .keys
            .get(&epoch)
            .ok_or_else(|| ProtectedKeyError::UnknownEpoch {
                group: group.clone(),
                epoch,
            })?;
        let entry = nodes
            .get_mut(node)
            .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?;
        let wrap_key = entry
            .wrap_key
            .as_ref()
            .ok_or_else(|| ProtectedKeyError::NodeRevoked { node: node.clone() })?;
        let aad = bind::release_ad(tenant, group, epoch, node);
        let wrapped_key = wrap_key.wrap(&aad, group_key.expose())?;
        entry.held.insert(group.clone(), epoch);
        tracing::debug!(node = %node, group = %group, epoch = %epoch, "released group key");
        Ok(KeyRelease {
            tenant: tenant.clone(),
            group: group.clone(),
            epoch,
            node: node.clone(),
            wrapped_key,
            lease_until: now.plus(tier.bound()),
        })
    }

    /// Revokes `node`: stops release to it, erases its wrapping path, and rotates every group
    /// it held so it cannot read data written after the revocation epoch.
    pub fn revoke_node(
        &mut self,
        node: &NodeId,
        now: MonoTime,
    ) -> Result<RevocationReport, ProtectedKeyError> {
        let tenant = self.tenant.clone();
        let held = {
            let entry = self
                .nodes
                .get_mut(node)
                .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?;
            if entry.status == NodeStatus::Revoked {
                return Err(ProtectedKeyError::NodeRevoked { node: node.clone() });
            }
            entry.status = NodeStatus::Revoked;
            entry.wrap_key = None;
            let held: Vec<GroupName> = entry.held.keys().cloned().collect();
            entry.held.clear();
            held
        };

        let mut epochs = BTreeMap::new();
        let mut rotated = Vec::new();
        for group in &held {
            let Some(from) = self.groups.get(group).map(|k| k.current) else {
                continue;
            };
            epochs.insert(group.clone(), from);
            let to = {
                let keys =
                    self.groups
                        .get_mut(group)
                        .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                            group: group.clone(),
                        })?;
                let next = keys
                    .current
                    .checked_next()
                    .ok_or(ProtectedKeyError::Malformed("epoch overflow"))?;
                keys.add_epoch(&tenant, group, &self.root, next)?;
                keys.rotated_at = Some(now);
                next
            };
            rotated.push(RotatedGroup {
                group: group.clone(),
                from,
                to,
            });
        }

        let entry = self
            .nodes
            .get_mut(node)
            .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?;
        entry.revocation = Some(Revocation { at: now, epochs });
        tracing::debug!(node = %node, groups = rotated.len(), "revoked node and rotated its groups");
        Ok(RevocationReport {
            node: node.clone(),
            revoked_at: now,
            rotated,
        })
    }

    /// The resumable plan to re-encrypt `group`'s older epochs under the current one.
    pub fn plan_reencrypt(
        &self,
        group: &GroupName,
        now: MonoTime,
    ) -> Result<ReencryptPlan, ProtectedKeyError> {
        let keys = self
            .groups
            .get(group)
            .ok_or_else(|| ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            })?;
        let target = keys.current;
        let batches = keys
            .keys
            .keys()
            .filter(|epoch| **epoch < target)
            .map(|epoch| ReencryptBatch {
                group: group.clone(),
                from: *epoch,
                to: target,
            })
            .collect();
        Ok(ReencryptPlan {
            group: group.clone(),
            target,
            batches,
            created_at: now,
        })
    }

    /// The key store in its wrapped form. Only wrapped keys are in it; never plaintext.
    #[must_use]
    pub fn backup(&self) -> KeyStoreBackup {
        let root = self.root.root();
        let mut groups = Vec::new();
        for (group, keys) in &self.groups {
            for (epoch, wrapped) in &keys.wrapped {
                groups.push(WrappedGroupKey {
                    group: group.clone(),
                    epoch: *epoch,
                    root: RootRef {
                        id: wrapped.key.id.clone(),
                        version: wrapped.key.version,
                    },
                    bytes: wrapped.bytes.clone(),
                });
            }
        }
        groups.sort_by(|a, b| a.group.cmp(&b.group).then(a.epoch.cmp(&b.epoch)));
        KeyStoreBackup {
            tenant: self.tenant.clone(),
            root: RootRef {
                id: root.key_ref().id.clone(),
                version: root.version(),
            },
            groups,
        }
    }

    /// Rebuilds a hub from a wrapped backup, unwrapping each group key under the root.
    pub fn restore(root: R, backup: KeyStoreBackup) -> Result<Self, ProtectedKeyError> {
        let tenant = backup.tenant.clone();
        if root.tenant() != &tenant {
            return Err(ProtectedKeyError::RootKey(
                "the backup belongs to another tenant".to_owned(),
            ));
        }
        let mut groups: BTreeMap<GroupName, GroupKeys> = BTreeMap::new();
        for wrapped in &backup.groups {
            let stored = WrappedKey {
                key: KeyRef {
                    id: wrapped.root.id.clone(),
                    version: wrapped.root.version,
                },
                bytes: wrapped.bytes.clone(),
            };
            let plain = root.unwrap(&stored, &root_aad(&tenant, &wrapped.group, wrapped.epoch))?;
            let key = LockedKey::from_bytes(plain.expose())?;
            let entry = groups
                .entry(wrapped.group.clone())
                .or_insert_with(GroupKeys::empty);
            entry.keys.insert(wrapped.epoch, key);
            entry.wrapped.insert(wrapped.epoch, stored);
            if wrapped.epoch > entry.current {
                entry.current = wrapped.epoch;
            }
        }
        Ok(Self {
            tenant,
            root,
            groups,
            nodes: BTreeMap::new(),
        })
    }
}
