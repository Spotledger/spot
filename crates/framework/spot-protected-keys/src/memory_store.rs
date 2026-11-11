//! [`MemoryKeyStore`]: the in-memory [`KeyStore`], the reference the PostgreSQL store is held to.
//!
//! It keeps wrapped forms only, exactly like the database store, and applies the same rules in
//! the same order, so the shared contract suite runs against both. It is for tests and
//! single-process use; it forgets everything when dropped.

use crate::error::ProtectedKeyError;
use crate::hub::{NodeStatus, RevocationReport, RotatedGroup};
use crate::ids::{GroupName, KeyEpoch, NodeId, TenantId};
use crate::store::{
    Audit, KeyEvent, KeyEventKind, KeyStore, KeyStoreBackup, KeyStoreError, MAX_EVENT_PAGE,
    MAX_ROTATIONS, RefusalReason, ReleaseRecord, RootRef, Rotation, StoredNode, WrappedGroupKey,
    WrappedNodeKey, check_epoch, check_root, check_wrapped,
};
use crate::time::MonoTime;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

struct GroupState {
    current: KeyEpoch,
    /// `None` is a retired epoch: the wrapped bytes are gone.
    keys: BTreeMap<KeyEpoch, Option<WrappedGroupKey>>,
}

#[derive(Default)]
struct State {
    groups: BTreeMap<GroupName, GroupState>,
    nodes: BTreeMap<NodeId, StoredNode>,
    holds: BTreeMap<(NodeId, GroupName), (KeyEpoch, MonoTime)>,
    revocations: BTreeMap<NodeId, RevocationReport>,
    events: Vec<KeyEvent>,
}

type Shared = Arc<Mutex<BTreeMap<TenantId, State>>>;

/// The in-memory key store of one tenant.
pub struct MemoryKeyStore {
    tenant: TenantId,
    shared: Shared,
}

impl std::fmt::Debug for MemoryKeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryKeyStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

/// What one event row carries besides the kind and the audit stamp.
#[derive(Default)]
struct Fields {
    group: Option<GroupName>,
    node: Option<NodeId>,
    epoch: Option<KeyEpoch>,
    to_epoch: Option<KeyEpoch>,
    detail: Option<String>,
}

impl State {
    fn push(&mut self, kind: KeyEventKind, fields: Fields, audit: &Audit) {
        let seq = u64::try_from(self.events.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.events.push(KeyEvent {
            seq,
            kind,
            group: fields.group,
            node: fields.node,
            epoch: fields.epoch,
            to_epoch: fields.to_epoch,
            detail: fields.detail,
            actor: audit.actor().to_owned(),
            at: audit.at(),
        });
    }

    fn revocation_epoch(&self, node: &NodeId, group: &GroupName) -> Option<KeyEpoch> {
        self.revocations
            .get(node)?
            .rotated
            .iter()
            .find(|r| &r.group == group)
            .map(|r| r.from)
    }
}

impl MemoryKeyStore {
    /// An empty store for `tenant`.
    #[must_use]
    pub fn new(tenant: TenantId) -> Self {
        Self {
            tenant,
            shared: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// A store for another tenant over the same backing data, as a second tenant of one
    /// database would be. The two never see each other's rows.
    #[must_use]
    pub fn sibling(&self, tenant: TenantId) -> Self {
        Self {
            tenant,
            shared: Arc::clone(&self.shared),
        }
    }

    fn with<R>(
        &self,
        f: impl FnOnce(&mut State) -> Result<R, KeyStoreError>,
    ) -> Result<R, KeyStoreError> {
        let mut all: MutexGuard<'_, BTreeMap<TenantId, State>> = self
            .shared
            .lock()
            .map_err(|_| KeyStoreError::Failed("the in-memory store is unavailable".to_owned()))?;
        f(all.entry(self.tenant.clone()).or_default())
    }
}

fn node_error(node: &NodeId, status: NodeStatus) -> Option<ProtectedKeyError> {
    match status {
        NodeStatus::Active => None,
        NodeStatus::Suspended => Some(ProtectedKeyError::NodeNotActive {
            node: node.clone(),
            status: "suspended",
        }),
        NodeStatus::Revoked => Some(ProtectedKeyError::NodeRevoked { node: node.clone() }),
    }
}

fn unknown_group(group: &GroupName) -> KeyStoreError {
    ProtectedKeyError::UnknownGroup {
        group: group.clone(),
    }
    .into()
}

fn check_time(at: MonoTime) -> Result<(), KeyStoreError> {
    if i64::try_from(at.millis()).is_err() {
        return Err(KeyStoreError::BadInput("the time is out of range"));
    }
    Ok(())
}

impl KeyStore for MemoryKeyStore {
    fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn create_group(&self, key: &WrappedGroupKey, audit: &Audit) -> Result<(), KeyStoreError> {
        check_epoch(key.epoch)?;
        check_wrapped(&key.bytes)?;
        check_root(&key.root)?;
        if key.epoch != KeyEpoch::FIRST {
            return Err(KeyStoreError::BadInput("a group starts at its first epoch"));
        }
        self.with(|s| {
            if s.groups.contains_key(&key.group) {
                return Err(ProtectedKeyError::GroupExists {
                    group: key.group.clone(),
                }
                .into());
            }
            let mut keys = BTreeMap::new();
            keys.insert(key.epoch, Some(key.clone()));
            s.groups.insert(
                key.group.clone(),
                GroupState {
                    current: key.epoch,
                    keys,
                },
            );
            s.push(
                KeyEventKind::Issue,
                Fields {
                    group: Some(key.group.clone()),
                    epoch: Some(key.epoch),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn rotate_group(
        &self,
        from: KeyEpoch,
        next: &WrappedGroupKey,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        check_epoch(from)?;
        check_epoch(next.epoch)?;
        check_wrapped(&next.bytes)?;
        check_root(&next.root)?;
        if from.checked_next() != Some(next.epoch) {
            return Err(KeyStoreError::BadInput(
                "the next epoch is not the one after the epoch read",
            ));
        }
        self.with(|s| {
            let group = s
                .groups
                .get_mut(&next.group)
                .ok_or_else(|| unknown_group(&next.group))?;
            if group.current != from {
                return Err(KeyStoreError::EpochConflict {
                    group: next.group.clone(),
                });
            }
            group.keys.insert(next.epoch, Some(next.clone()));
            group.current = next.epoch;
            s.push(
                KeyEventKind::Rotate,
                Fields {
                    group: Some(next.group.clone()),
                    epoch: Some(from),
                    to_epoch: Some(next.epoch),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn group_epoch(&self, group: &GroupName) -> Result<Option<KeyEpoch>, KeyStoreError> {
        self.with(|s| Ok(s.groups.get(group).map(|g| g.current)))
    }

    fn groups(&self) -> Result<Vec<GroupName>, KeyStoreError> {
        self.with(|s| Ok(s.groups.keys().cloned().collect()))
    }

    fn group_key(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
    ) -> Result<WrappedGroupKey, KeyStoreError> {
        self.with(|s| {
            let state = s.groups.get(group).ok_or_else(|| unknown_group(group))?;
            match state.keys.get(&epoch) {
                None => Err(ProtectedKeyError::UnknownEpoch {
                    group: group.clone(),
                    epoch,
                }
                .into()),
                Some(None) => Err(ProtectedKeyError::EpochRetired {
                    group: group.clone(),
                    epoch,
                }
                .into()),
                Some(Some(key)) => Ok(key.clone()),
            }
        })
    }

    fn retire_epoch(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        self.with(|s| {
            let state = s
                .groups
                .get_mut(group)
                .ok_or_else(|| unknown_group(group))?;
            if epoch == state.current {
                return Err(KeyStoreError::BadInput(
                    "the current epoch cannot be retired",
                ));
            }
            match state.keys.get_mut(&epoch) {
                None => {
                    return Err(ProtectedKeyError::UnknownEpoch {
                        group: group.clone(),
                        epoch,
                    }
                    .into());
                }
                Some(slot) if slot.is_none() => {
                    return Err(ProtectedKeyError::EpochRetired {
                        group: group.clone(),
                        epoch,
                    }
                    .into());
                }
                Some(slot) => *slot = None,
            }
            s.push(
                KeyEventKind::Retire,
                Fields {
                    group: Some(group.clone()),
                    epoch: Some(epoch),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn backup(&self, root: &RootRef) -> Result<KeyStoreBackup, KeyStoreError> {
        check_root(root)?;
        let tenant = self.tenant.clone();
        self.with(|s| {
            let groups = s
                .groups
                .values()
                .flat_map(|g| g.keys.values().flatten().cloned())
                .collect();
            Ok(KeyStoreBackup {
                tenant,
                root: root.clone(),
                groups,
            })
        })
    }

    fn register_node(
        &self,
        node: &NodeId,
        wrap: &WrappedNodeKey,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        check_wrapped(&wrap.bytes)?;
        check_root(&wrap.root)?;
        self.with(|s| {
            let registered_at = match s.nodes.get(node) {
                Some(existing) if existing.status == NodeStatus::Revoked => {
                    return Err(ProtectedKeyError::NodeRevoked { node: node.clone() }.into());
                }
                Some(existing) => existing.registered_at,
                None => audit.at(),
            };
            s.nodes.insert(
                node.clone(),
                StoredNode {
                    node: node.clone(),
                    status: NodeStatus::Active,
                    wrap: Some(wrap.clone()),
                    registered_at,
                },
            );
            s.push(
                KeyEventKind::Register,
                Fields {
                    node: Some(node.clone()),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn node(&self, node: &NodeId) -> Result<Option<StoredNode>, KeyStoreError> {
        self.with(|s| Ok(s.nodes.get(node).cloned()))
    }

    fn set_node_status(
        &self,
        node: &NodeId,
        status: NodeStatus,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        if status == NodeStatus::Revoked {
            return Err(KeyStoreError::BadInput("revoke a node with revoke_node"));
        }
        self.with(|s| {
            let entry = s
                .nodes
                .get_mut(node)
                .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?;
            if entry.status == NodeStatus::Revoked {
                return Err(ProtectedKeyError::NodeRevoked { node: node.clone() }.into());
            }
            entry.status = status;
            s.push(
                KeyEventKind::Status,
                Fields {
                    node: Some(node.clone()),
                    detail: Some(status.as_str().to_owned()),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn record_release(&self, release: &ReleaseRecord, audit: &Audit) -> Result<(), KeyStoreError> {
        check_epoch(release.epoch)?;
        check_time(release.lease_until)?;
        self.with(|s| {
            let status = s
                .nodes
                .get(&release.node)
                .ok_or_else(|| ProtectedKeyError::UnknownNode {
                    node: release.node.clone(),
                })?
                .status;
            if let Some(error) = node_error(&release.node, status) {
                return Err(error.into());
            }
            if s.revocation_epoch(&release.node, &release.group)
                .is_some_and(|revoked| release.epoch >= revoked)
            {
                return Err(ProtectedKeyError::NodeRevoked {
                    node: release.node.clone(),
                }
                .into());
            }
            let group = s
                .groups
                .get(&release.group)
                .ok_or_else(|| unknown_group(&release.group))?;
            match group.keys.get(&release.epoch) {
                None => {
                    return Err(ProtectedKeyError::UnknownEpoch {
                        group: release.group.clone(),
                        epoch: release.epoch,
                    }
                    .into());
                }
                Some(None) => {
                    return Err(ProtectedKeyError::EpochRetired {
                        group: release.group.clone(),
                        epoch: release.epoch,
                    }
                    .into());
                }
                Some(Some(_)) => {}
            }
            s.holds.insert(
                (release.node.clone(), release.group.clone()),
                (release.epoch, release.lease_until),
            );
            s.push(
                KeyEventKind::Release,
                Fields {
                    group: Some(release.group.clone()),
                    node: Some(release.node.clone()),
                    epoch: Some(release.epoch),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn record_refusal(
        &self,
        node: &NodeId,
        group: &GroupName,
        epoch: Option<KeyEpoch>,
        reason: RefusalReason,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        if let Some(epoch) = epoch {
            check_epoch(epoch)?;
        }
        self.with(|s| {
            s.push(
                KeyEventKind::Refuse,
                Fields {
                    group: Some(group.clone()),
                    node: Some(node.clone()),
                    epoch,
                    detail: Some(reason.code().to_owned()),
                    ..Fields::default()
                },
                audit,
            );
            Ok(())
        })
    }

    fn held(&self, node: &NodeId) -> Result<Vec<ReleaseRecord>, KeyStoreError> {
        self.with(|s| {
            Ok(s.holds
                .iter()
                .filter(|((n, _), _)| n == node)
                .map(|((n, g), (epoch, lease))| ReleaseRecord {
                    node: n.clone(),
                    group: g.clone(),
                    epoch: *epoch,
                    lease_until: *lease,
                })
                .collect())
        })
    }

    fn revoke_node(
        &self,
        node: &NodeId,
        rotations: &[Rotation],
        audit: &Audit,
    ) -> Result<RevocationReport, KeyStoreError> {
        if rotations.len() > MAX_ROTATIONS {
            return Err(KeyStoreError::BadInput("too many groups in one revocation"));
        }
        let mut rotations = rotations.to_vec();
        rotations.sort_by(|a, b| a.next.group.cmp(&b.next.group));
        let mut named = BTreeSet::new();
        for rotation in &rotations {
            check_epoch(rotation.from)?;
            check_epoch(rotation.next.epoch)?;
            check_wrapped(&rotation.next.bytes)?;
            check_root(&rotation.next.root)?;
            if rotation.from.checked_next() != Some(rotation.next.epoch) {
                return Err(KeyStoreError::BadInput(
                    "the next epoch is not the one after the epoch read",
                ));
            }
            if !named.insert(rotation.next.group.clone()) {
                return Err(KeyStoreError::BadInput("a group is rotated twice"));
            }
        }
        self.with(|s| {
            let status = s
                .nodes
                .get(node)
                .ok_or_else(|| ProtectedKeyError::UnknownNode { node: node.clone() })?
                .status;
            if status == NodeStatus::Revoked {
                return Err(ProtectedKeyError::NodeRevoked { node: node.clone() }.into());
            }
            let held: BTreeSet<GroupName> = s
                .holds
                .keys()
                .filter(|(n, _)| n == node)
                .map(|(_, g)| g.clone())
                .collect();
            if named.iter().any(|g| !held.contains(g)) {
                return Err(KeyStoreError::BadInput(
                    "a rotation names a group the node does not hold",
                ));
            }
            if let Some(missed) = held.iter().find(|g| !named.contains(*g)) {
                return Err(KeyStoreError::EpochConflict {
                    group: missed.clone(),
                });
            }
            for rotation in &rotations {
                let group = s
                    .groups
                    .get(&rotation.next.group)
                    .ok_or_else(|| unknown_group(&rotation.next.group))?;
                if group.current != rotation.from {
                    return Err(KeyStoreError::EpochConflict {
                        group: rotation.next.group.clone(),
                    });
                }
            }
            // Every check passed: apply.
            let mut rotated = Vec::with_capacity(rotations.len());
            for rotation in &rotations {
                if let Some(group) = s.groups.get_mut(&rotation.next.group) {
                    group
                        .keys
                        .insert(rotation.next.epoch, Some(rotation.next.clone()));
                    group.current = rotation.next.epoch;
                }
                s.push(
                    KeyEventKind::Rotate,
                    Fields {
                        group: Some(rotation.next.group.clone()),
                        node: Some(node.clone()),
                        epoch: Some(rotation.from),
                        to_epoch: Some(rotation.next.epoch),
                        ..Fields::default()
                    },
                    audit,
                );
                rotated.push(RotatedGroup {
                    group: rotation.next.group.clone(),
                    from: rotation.from,
                    to: rotation.next.epoch,
                });
            }
            rotated.sort_by(|a, b| a.group.cmp(&b.group));
            if let Some(entry) = s.nodes.get_mut(node) {
                entry.status = NodeStatus::Revoked;
                entry.wrap = None;
            }
            s.holds.retain(|(n, _), _| n != node);
            let report = RevocationReport {
                node: node.clone(),
                revoked_at: audit.at(),
                rotated,
            };
            s.revocations.insert(node.clone(), report.clone());
            s.push(
                KeyEventKind::Revoke,
                Fields {
                    node: Some(node.clone()),
                    ..Fields::default()
                },
                audit,
            );
            Ok(report)
        })
    }

    fn revocation_epoch(
        &self,
        node: &NodeId,
        group: &GroupName,
    ) -> Result<Option<KeyEpoch>, KeyStoreError> {
        self.with(|s| Ok(s.revocation_epoch(node, group)))
    }

    fn events(&self, after: u64, limit: usize) -> Result<Vec<KeyEvent>, KeyStoreError> {
        if limit == 0 || limit > MAX_EVENT_PAGE {
            return Err(KeyStoreError::BadInput("the page size is out of range"));
        }
        self.with(|s| {
            Ok(s.events
                .iter()
                .filter(|e| e.seq > after)
                .take(limit)
                .cloned()
                .collect())
        })
    }
}
