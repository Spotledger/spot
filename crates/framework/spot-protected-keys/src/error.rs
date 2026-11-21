//! The one error type of the crate and its stable codes (`protected_keys.toml`).
//!
//! A message names ids, epochs and groups only: never key material, never a wrapped key's
//! bytes. Every code is catalogued in `crates/framework/spot-errors/catalogue/protected_keys.toml`.

use crate::ids::{GroupName, KeyEpoch, NodeId};
use thiserror::Error;

/// The group has no key.
pub const UNKNOWN_GROUP: &str = "protected_keys.unknown_group";
/// A group with this name already exists.
pub const GROUP_EXISTS: &str = "protected_keys.group_exists";
/// The group has no key at this epoch.
pub const UNKNOWN_EPOCH: &str = "protected_keys.unknown_epoch";
/// The epoch's key material is gone (retired after re-encryption).
pub const EPOCH_RETIRED: &str = "protected_keys.epoch_retired";
/// The node is not registered.
pub const UNKNOWN_NODE: &str = "protected_keys.unknown_node";
/// The node is suspended.
pub const NODE_NOT_ACTIVE: &str = "protected_keys.node_not_active";
/// The node is revoked; release to it is refused.
pub const NODE_REVOKED: &str = "protected_keys.node_revoked";
/// The node has no permission-snapshot tier for the group.
pub const NO_PERMISSION: &str = "protected_keys.no_permission";
/// A release addressed to a different node.
pub const WRONG_NODE: &str = "protected_keys.wrong_node";
/// The key lease has passed.
pub const LEASE_EXPIRED: &str = "protected_keys.lease_expired";
/// A wrapped key or ciphertext failed authentication.
pub const TAMPERED: &str = "protected_keys.tampered";
/// A wrapped key or ciphertext is not in the expected shape.
pub const MALFORMED: &str = "protected_keys.malformed";
/// The root key provider failed (I/O, bad configuration, wrong key).
pub const ROOT_KEY: &str = "protected_keys.root_key";
/// An id, a group name or a field name is not valid.
pub const BAD_ID: &str = "protected_keys.bad_id";

/// A failure of the protected-group key hierarchy. Messages never contain key material.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtectedKeyError {
    /// The group has no key.
    #[error("protected_keys.unknown_group: no protection group {group}")]
    UnknownGroup {
        /// The group name.
        group: GroupName,
    },
    /// A group with this name already exists.
    #[error("protected_keys.group_exists: protection group {group} already exists")]
    GroupExists {
        /// The group name.
        group: GroupName,
    },
    /// The group has no key at this epoch.
    #[error("protected_keys.unknown_epoch: group {group} has no key at epoch {epoch}")]
    UnknownEpoch {
        /// The group name.
        group: GroupName,
        /// The missing epoch.
        epoch: KeyEpoch,
    },
    /// The epoch's key material is gone.
    #[error("protected_keys.epoch_retired: group {group} epoch {epoch} is retired")]
    EpochRetired {
        /// The group name.
        group: GroupName,
        /// The retired epoch.
        epoch: KeyEpoch,
    },
    /// The node is not registered.
    #[error("protected_keys.unknown_node: no node {node}")]
    UnknownNode {
        /// The node id.
        node: NodeId,
    },
    /// The node is not active.
    #[error("protected_keys.node_not_active: node {node} is {status}")]
    NodeNotActive {
        /// The node id.
        node: NodeId,
        /// `suspended` or `revoked`.
        status: &'static str,
    },
    /// The node is revoked.
    #[error("protected_keys.node_revoked: node {node} is revoked")]
    NodeRevoked {
        /// The node id.
        node: NodeId,
    },
    /// The node has no tier for the group.
    #[error("protected_keys.no_permission: node {node} has no tier for group {group}")]
    NoPermission {
        /// The node id.
        node: NodeId,
        /// The group name.
        group: GroupName,
    },
    /// A release addressed to a different node.
    #[error("protected_keys.wrong_node: the release is for node {addressed}, not {node}")]
    WrongNode {
        /// The node that received the release.
        node: NodeId,
        /// The node the release was addressed to.
        addressed: NodeId,
    },
    /// The lease has passed.
    #[error("protected_keys.lease_expired: the key lease for group {group} has passed")]
    LeaseExpired {
        /// The group name.
        group: GroupName,
    },
    /// Authentication failed.
    #[error("protected_keys.tampered: authentication failed")]
    Tampered,
    /// A value is not in the expected shape.
    #[error("protected_keys.malformed: {0}")]
    Malformed(&'static str),
    /// The root key provider failed.
    #[error("protected_keys.root_key: {0}")]
    RootKey(String),
    /// An id is not valid.
    #[error("protected_keys.bad_id: {kind} is not valid")]
    BadId {
        /// What kind of id (for example `group name`).
        kind: &'static str,
    },
}

impl ProtectedKeyError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownGroup { .. } => UNKNOWN_GROUP,
            Self::GroupExists { .. } => GROUP_EXISTS,
            Self::UnknownEpoch { .. } => UNKNOWN_EPOCH,
            Self::EpochRetired { .. } => EPOCH_RETIRED,
            Self::UnknownNode { .. } => UNKNOWN_NODE,
            Self::NodeNotActive { .. } => NODE_NOT_ACTIVE,
            Self::NodeRevoked { .. } => NODE_REVOKED,
            Self::NoPermission { .. } => NO_PERMISSION,
            Self::WrongNode { .. } => WRONG_NODE,
            Self::LeaseExpired { .. } => LEASE_EXPIRED,
            Self::Tampered => TAMPERED,
            Self::Malformed(_) => MALFORMED,
            Self::RootKey(_) => ROOT_KEY,
            Self::BadId { .. } => BAD_ID,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn probe() -> Vec<ProtectedKeyError> {
        let group = GroupName::new("bank_details").expect("group");
        let node = NodeId::new("branch-1").expect("node");
        vec![
            ProtectedKeyError::UnknownGroup {
                group: group.clone(),
            },
            ProtectedKeyError::GroupExists {
                group: group.clone(),
            },
            ProtectedKeyError::UnknownEpoch {
                group: group.clone(),
                epoch: KeyEpoch::FIRST,
            },
            ProtectedKeyError::EpochRetired {
                group: group.clone(),
                epoch: KeyEpoch::FIRST,
            },
            ProtectedKeyError::UnknownNode { node: node.clone() },
            ProtectedKeyError::NodeNotActive {
                node: node.clone(),
                status: "suspended",
            },
            ProtectedKeyError::NodeRevoked { node: node.clone() },
            ProtectedKeyError::NoPermission {
                node: node.clone(),
                group: group.clone(),
            },
            ProtectedKeyError::WrongNode {
                node: node.clone(),
                addressed: NodeId::new("branch-2").expect("node"),
            },
            ProtectedKeyError::LeaseExpired {
                group: group.clone(),
            },
            ProtectedKeyError::Tampered,
            ProtectedKeyError::Malformed("truncated"),
            ProtectedKeyError::RootKey("kms unreachable".to_owned()),
            ProtectedKeyError::BadId { kind: "group name" },
        ]
    }

    #[test]
    fn every_code_has_a_catalogue_entry() {
        let catalogue = spot_errors::Catalogue::load();
        assert_eq!(
            catalogue.load_error(),
            None,
            "the catalogue must load clean"
        );
        let mut seen = std::collections::BTreeSet::new();
        for error in probe() {
            let code = error.code();
            assert!(code.starts_with("protected_keys."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        assert_eq!(seen.len(), 14, "a variant is missing from the probe");
    }

    #[test]
    fn messages_carry_no_key_material() {
        for error in probe() {
            let text = error.to_string();
            assert!(text.contains(error.code()), "{text}");
            assert!(!text.contains("key="), "{text}");
        }
    }
}
