//! [`KeyStoreBackup`]: the key store's at-rest shape, which holds only wrapped keys.
//!
//! A database dump or a backup of the key store contains group keys wrapped under the tenant
//! root key, and nothing else. There is no plaintext key anywhere in this type, so a backup
//! without the root key reveals no group key (ADR-070 rule 12.13 e).

use crate::ids::{GroupName, KeyEpoch, TenantId};
use serde::{Deserialize, Serialize};

/// A key-encryption-key reference: the id and version of the root key that wrapped a group key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRef {
    /// The KEK id (a KMS key id, a key file name).
    pub id: String,
    /// The KEK version.
    pub version: u32,
}

/// A group data key in its wrapped form, safe to store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedGroupKey {
    /// The protection group.
    pub group: GroupName,
    /// The epoch.
    pub epoch: KeyEpoch,
    /// The root key that wrapped it.
    pub root: RootRef,
    /// The wrapped bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for WrappedGroupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrappedGroupKey")
            .field("group", &self.group)
            .field("epoch", &self.epoch)
            .field("root", &self.root)
            .field("bytes", &"[wrapped]")
            .finish()
    }
}

/// The whole key store in its wrapped form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyStoreBackup {
    /// The tenant.
    pub tenant: TenantId,
    /// The root key the store was wrapped under.
    pub root: RootRef,
    /// Every group key, wrapped, one per epoch.
    pub groups: Vec<WrappedGroupKey>,
}

impl KeyStoreBackup {
    /// The wrapped key for `group` at `epoch`, if present.
    #[must_use]
    pub fn get(&self, group: &GroupName, epoch: KeyEpoch) -> Option<&WrappedGroupKey> {
        self.groups
            .iter()
            .find(|k| &k.group == group && k.epoch == epoch)
    }
}

// ---------------------------------------------------------------------------------------------
// The persistence port (WP-SEC-10.T4).
//
// A store holds wrapped forms only (a group key wrapped under the tenant root, a node wrapping
// key wrapped under the tenant root), the epoch head of every group, node status, leases,
// revocation records and an append-only `KeyEvent` history. It never sees a plaintext key, and
// it is bound to ONE tenant for its whole life: no method takes a tenant. The in-memory
// implementation is `crate::MemoryKeyStore`; the PostgreSQL one is `spot-protected-keys-pg`.
// ---------------------------------------------------------------------------------------------

use crate::error::ProtectedKeyError;
use crate::hub::{NodeStatus, RevocationReport};
use crate::ids::NodeId;
use crate::time::MonoTime;
use thiserror::Error;

/// The shortest a wrapped key may be: the AEAD nonce and tag alone are 40 bytes, so a bare
/// 32-byte key can never pass for a wrapped one.
pub const MIN_WRAPPED_LEN: usize = 40;
/// The longest a wrapped key may be (a KMS envelope with its header).
pub const MAX_WRAPPED_LEN: usize = 4096;
/// The longest an actor name or a root key id may be.
pub const MAX_TEXT_LEN: usize = 128;
/// The most events one page returns.
pub const MAX_EVENT_PAGE: usize = 500;
/// The most groups one revocation may rotate.
pub const MAX_ROTATIONS: usize = 1024;

/// The epoch changed under the caller: a compare-and-set lost, or a revocation raced a release.
pub const EPOCH_CONFLICT: &str = "protected_keys_pg.epoch_conflict";
/// An argument is outside the bounds or shape the store accepts.
pub const BAD_INPUT: &str = "protected_keys_pg.bad_input";
/// The backend failed; nothing was written that the caller can rely on.
pub const STORE_FAILED: &str = "protected_keys_pg.store_failed";
/// A stored row cannot be read back or disagrees with itself.
pub const CORRUPT: &str = "protected_keys_pg.corrupt";

/// A failure of a [`KeyStore`]. The set is closed. A message names ids, groups and epochs, or is
/// a fixed sentence; it never carries a driver message, SQL, a parameter or key material.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyStoreError {
    /// A hierarchy rule refused the call (unknown group, revoked node, retired epoch, ...).
    #[error("{0}")]
    Key(ProtectedKeyError),
    /// The group's epoch is no longer what the caller read: another hub rotated first.
    #[error("protected_keys_pg.epoch_conflict: the epoch of group {group} changed; read it again")]
    EpochConflict {
        /// The group.
        group: GroupName,
    },
    /// An argument is not acceptable.
    #[error("protected_keys_pg.bad_input: {0}")]
    BadInput(&'static str),
    /// The backend failed.
    #[error("protected_keys_pg.store_failed: {0}")]
    Failed(String),
    /// A stored row is not readable.
    #[error("protected_keys_pg.corrupt: a stored row is not readable ({0})")]
    Corrupt(&'static str),
}

impl KeyStoreError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Key(inner) => inner.code(),
            Self::EpochConflict { .. } => EPOCH_CONFLICT,
            Self::BadInput(_) => BAD_INPUT,
            Self::Failed(_) => STORE_FAILED,
            Self::Corrupt(_) => CORRUPT,
        }
    }
}

impl From<ProtectedKeyError> for KeyStoreError {
    fn from(value: ProtectedKeyError) -> Self {
        Self::Key(value)
    }
}

fn clean_text(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= MAX_TEXT_LEN && !raw.chars().any(char::is_control)
}

/// Checks a wrapped key's length, which both implementations apply before they store it.
///
/// # Errors
/// [`KeyStoreError::BadInput`] when it is shorter than [`MIN_WRAPPED_LEN`] (a bare key) or
/// longer than [`MAX_WRAPPED_LEN`].
pub fn check_wrapped(bytes: &[u8]) -> Result<(), KeyStoreError> {
    if bytes.len() < MIN_WRAPPED_LEN || bytes.len() > MAX_WRAPPED_LEN {
        return Err(KeyStoreError::BadInput(
            "a wrapped key has an unusable length",
        ));
    }
    Ok(())
}

/// Checks a root key reference.
///
/// # Errors
/// [`KeyStoreError::BadInput`] for an empty, long or control-character id.
pub fn check_root(root: &RootRef) -> Result<(), KeyStoreError> {
    if clean_text(&root.id) {
        Ok(())
    } else {
        Err(KeyStoreError::BadInput("the root key id is not usable"))
    }
}

/// Checks an epoch: zero is the absent epoch and is never stored.
///
/// # Errors
/// [`KeyStoreError::BadInput`] for epoch zero or one beyond the signed 64-bit range.
pub fn check_epoch(epoch: KeyEpoch) -> Result<(), KeyStoreError> {
    if epoch.get() == 0 || i64::try_from(epoch.get()).is_err() {
        return Err(KeyStoreError::BadInput("the epoch is out of range"));
    }
    Ok(())
}

/// Who did it and when (on the hub's monotonic clock): stamped on the event a write appends.
/// The store reads no clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audit {
    actor: String,
    at: MonoTime,
}

impl Audit {
    /// An audit stamp. The actor is an id (a principal or service name), never a secret.
    ///
    /// # Errors
    /// [`KeyStoreError::BadInput`] when the actor is empty, longer than [`MAX_TEXT_LEN`], has a
    /// control character, or `at` is beyond the signed 64-bit range.
    pub fn new(actor: &str, at: MonoTime) -> Result<Self, KeyStoreError> {
        if !clean_text(actor) {
            return Err(KeyStoreError::BadInput("the actor is not usable"));
        }
        if i64::try_from(at.millis()).is_err() {
            return Err(KeyStoreError::BadInput("the time is out of range"));
        }
        Ok(Self {
            actor: actor.to_owned(),
            at,
        })
    }

    /// The actor.
    #[must_use]
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// The instant.
    #[must_use]
    pub fn at(&self) -> MonoTime {
        self.at
    }
}

/// What a [`KeyEvent`] records. The set is closed; the text form is what the table stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyEventKind {
    /// A group was created at its first epoch.
    Issue,
    /// A group moved to its next epoch (`epoch` is the one superseded, `to_epoch` the new one).
    Rotate,
    /// A group key was released to a node under a lease.
    Release,
    /// A release was refused (`detail` holds the stable code).
    Refuse,
    /// A node was revoked.
    Revoke,
    /// An epoch's wrapped key was erased after re-encryption.
    Retire,
    /// A node was registered, or its wrapping key changed.
    Register,
    /// A node was suspended or reactivated (`detail` holds the new status).
    Status,
}

impl KeyEventKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Rotate => "rotate",
            Self::Release => "release",
            Self::Refuse => "refuse",
            Self::Revoke => "revoke",
            Self::Retire => "retire",
            Self::Register => "register",
            Self::Status => "status",
        }
    }

    /// The kind with this text form, if any.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "issue" => Self::Issue,
            "rotate" => Self::Rotate,
            "release" => Self::Release,
            "refuse" => Self::Refuse,
            "revoke" => Self::Revoke,
            "retire" => Self::Retire,
            "register" => Self::Register,
            "status" => Self::Status,
            _ => return None,
        })
    }
}

/// One row of the append-only key history. It names ids, epochs and a stable code, never key
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// The position in the tenant's history, increasing.
    pub seq: u64,
    /// What happened.
    pub kind: KeyEventKind,
    /// The group, when the event concerns one.
    pub group: Option<GroupName>,
    /// The node, when the event concerns one.
    pub node: Option<NodeId>,
    /// The epoch concerned.
    pub epoch: Option<KeyEpoch>,
    /// The epoch moved to (rotation only).
    pub to_epoch: Option<KeyEpoch>,
    /// A stable code or status word, or `None`.
    pub detail: Option<String>,
    /// Who did it.
    pub actor: String,
    /// When, on the hub's monotonic clock.
    pub at: MonoTime,
}

/// Why a release was refused, for the history. The set is closed to the hierarchy's own codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// `protected_keys.unknown_node`.
    UnknownNode,
    /// `protected_keys.node_not_active`.
    NodeNotActive,
    /// `protected_keys.node_revoked`.
    NodeRevoked,
    /// `protected_keys.no_permission`.
    NoPermission,
    /// `protected_keys.unknown_group`.
    UnknownGroup,
    /// `protected_keys.unknown_epoch`.
    UnknownEpoch,
    /// `protected_keys.epoch_retired`.
    EpochRetired,
}

impl RefusalReason {
    const ALL: [Self; 7] = [
        Self::UnknownNode,
        Self::NodeNotActive,
        Self::NodeRevoked,
        Self::NoPermission,
        Self::UnknownGroup,
        Self::UnknownEpoch,
        Self::EpochRetired,
    ];

    /// The stable code the event records.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownNode => crate::error::UNKNOWN_NODE,
            Self::NodeNotActive => crate::error::NODE_NOT_ACTIVE,
            Self::NodeRevoked => crate::error::NODE_REVOKED,
            Self::NoPermission => crate::error::NO_PERMISSION,
            Self::UnknownGroup => crate::error::UNKNOWN_GROUP,
            Self::UnknownEpoch => crate::error::UNKNOWN_EPOCH,
            Self::EpochRetired => crate::error::EPOCH_RETIRED,
        }
    }

    /// The reason a hierarchy error stands for, if it is a release refusal.
    #[must_use]
    pub fn of(error: &ProtectedKeyError) -> Option<Self> {
        Some(match error {
            ProtectedKeyError::UnknownNode { .. } => Self::UnknownNode,
            ProtectedKeyError::NodeNotActive { .. } => Self::NodeNotActive,
            ProtectedKeyError::NodeRevoked { .. } => Self::NodeRevoked,
            ProtectedKeyError::NoPermission { .. } => Self::NoPermission,
            ProtectedKeyError::UnknownGroup { .. } => Self::UnknownGroup,
            ProtectedKeyError::UnknownEpoch { .. } => Self::UnknownEpoch,
            ProtectedKeyError::EpochRetired { .. } => Self::EpochRetired,
            _ => return None,
        })
    }

    /// The reason with this stable code, if any.
    #[must_use]
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.code() == code)
    }
}

/// A node's wrapping key in its at-rest form: wrapped under the tenant root key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedNodeKey {
    /// The root key that wrapped it.
    pub root: RootRef,
    /// The wrapped bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for WrappedNodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrappedNodeKey")
            .field("root", &self.root)
            .field("bytes", &"[wrapped]")
            .finish()
    }
}

/// A node as the store keeps it. After revocation `wrap` is `None`: the wrapping path is erased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredNode {
    /// The node.
    pub node: NodeId,
    /// Active, suspended or revoked.
    pub status: NodeStatus,
    /// The wrapped node key, until revocation.
    pub wrap: Option<WrappedNodeKey>,
    /// When it was registered, on the hub's monotonic clock.
    pub registered_at: MonoTime,
}

/// A key released to a node: which epoch of which group, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseRecord {
    /// The node.
    pub node: NodeId,
    /// The group.
    pub group: GroupName,
    /// The epoch released.
    pub epoch: KeyEpoch,
    /// The end of the lease on the hub's monotonic clock.
    pub lease_until: MonoTime,
}

/// One group's step in a revocation: the epoch in force and the wrapped key of the next one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rotation {
    /// The epoch in force (the head the compare-and-set expects).
    pub from: KeyEpoch,
    /// The new epoch's wrapped key, which must be `from` plus one.
    pub next: WrappedGroupKey,
}

/// The persistence port of the key hierarchy, bound to one tenant.
///
/// Rules every implementation keeps (the shared contract suite proves them):
///
/// - only wrapped forms are accepted and returned;
/// - the epoch head moves by **compare-and-set**: [`KeyStore::rotate_group`] and
///   [`KeyStore::revoke_node`] name the epoch they expect, and the loser of a race gets
///   [`KeyStoreError::EpochConflict`] and writes nothing;
/// - a write and its history event commit together or not at all;
/// - [`KeyStore::record_release`] succeeds only for an active node, a live epoch and an epoch
///   earlier than the node's revocation epoch; callers hand the wrapped key out only after it
///   returned `Ok`;
/// - a revoked node stays revoked, and its wrapped key is erased;
/// - a failure is an error, never an empty answer.
pub trait KeyStore: Send + Sync {
    /// The tenant every call is scoped to.
    fn tenant(&self) -> &TenantId;

    /// Creates `key.group` at [`KeyEpoch::FIRST`].
    ///
    /// # Errors
    /// `GroupExists` if the group is there; `BadInput` if the epoch is not the first one.
    fn create_group(&self, key: &WrappedGroupKey, audit: &Audit) -> Result<(), KeyStoreError>;

    /// Moves `next.group` from epoch `from` to `from + 1`, storing `next`.
    ///
    /// # Errors
    /// `UnknownGroup`; [`KeyStoreError::EpochConflict`] when the head is not `from`.
    fn rotate_group(
        &self,
        from: KeyEpoch,
        next: &WrappedGroupKey,
        audit: &Audit,
    ) -> Result<(), KeyStoreError>;

    /// The group's current epoch, or `None` when it does not exist.
    fn group_epoch(&self, group: &GroupName) -> Result<Option<KeyEpoch>, KeyStoreError>;

    /// Every group, in name order.
    fn groups(&self) -> Result<Vec<GroupName>, KeyStoreError>;

    /// The wrapped key of `group` at `epoch`.
    ///
    /// # Errors
    /// `UnknownGroup`, `UnknownEpoch`, or `EpochRetired` once the material was erased.
    fn group_key(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
    ) -> Result<WrappedGroupKey, KeyStoreError>;

    /// Erases the wrapped key of an old epoch after its data was re-encrypted. The current epoch
    /// cannot be retired.
    fn retire_epoch(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
        audit: &Audit,
    ) -> Result<(), KeyStoreError>;

    /// Every live (not retired) wrapped group key, as the [`KeyStoreBackup`] a hub restores from.
    fn backup(&self, root: &RootRef) -> Result<KeyStoreBackup, KeyStoreError>;

    /// Registers `node` with its wrapped key, or replaces the key of a node that is not revoked
    /// (status becomes active; holds are kept).
    ///
    /// # Errors
    /// `NodeRevoked` for a revoked node.
    fn register_node(
        &self,
        node: &NodeId,
        wrap: &WrappedNodeKey,
        audit: &Audit,
    ) -> Result<(), KeyStoreError>;

    /// The node, or `None`.
    fn node(&self, node: &NodeId) -> Result<Option<StoredNode>, KeyStoreError>;

    /// Suspends or reactivates a node.
    ///
    /// # Errors
    /// `UnknownNode`; `NodeRevoked`; `BadInput` for [`NodeStatus::Revoked`] (use
    /// [`KeyStore::revoke_node`]).
    fn set_node_status(
        &self,
        node: &NodeId,
        status: NodeStatus,
        audit: &Audit,
    ) -> Result<(), KeyStoreError>;

    /// Records a release and its lease.
    ///
    /// # Errors
    /// `UnknownNode`, `NodeNotActive`, `NodeRevoked` (also for an epoch at or after the node's
    /// revocation epoch), `UnknownGroup`, `UnknownEpoch`, `EpochRetired`.
    fn record_release(&self, release: &ReleaseRecord, audit: &Audit) -> Result<(), KeyStoreError>;

    /// Records that a release was refused.
    fn record_refusal(
        &self,
        node: &NodeId,
        group: &GroupName,
        epoch: Option<KeyEpoch>,
        reason: RefusalReason,
        audit: &Audit,
    ) -> Result<(), KeyStoreError>;

    /// The keys currently released to `node`, one per group, by group.
    fn held(&self, node: &NodeId) -> Result<Vec<ReleaseRecord>, KeyStoreError>;

    /// Revokes `node` and rotates, in one atomic step, exactly the groups it holds.
    ///
    /// # Errors
    /// `UnknownNode`, `NodeRevoked` (already); [`KeyStoreError::EpochConflict`] when a group's
    /// head is not the `from` named, or the node holds a group `rotations` does not cover
    /// (read [`KeyStore::held`] again); `BadInput` for a rotation of a group the node does not
    /// hold. Nothing is written on an error.
    fn revoke_node(
        &self,
        node: &NodeId,
        rotations: &[Rotation],
        audit: &Audit,
    ) -> Result<RevocationReport, KeyStoreError>;

    /// The epoch in force for `group` when `node` was revoked, if it was and held the group.
    fn revocation_epoch(
        &self,
        node: &NodeId,
        group: &GroupName,
    ) -> Result<Option<KeyEpoch>, KeyStoreError>;

    /// The events after `after` (0 for the start), oldest first, at most `limit`
    /// (1 to [`MAX_EVENT_PAGE`]).
    fn events(&self, after: u64, limit: usize) -> Result<Vec<KeyEvent>, KeyStoreError>;
}
