//! The one error type of the crate and its stable codes (`sealed_fields.toml`).
//!
//! A message names a group, a field or a node only: never a value, a key or a sealed blob.
//! Every code is catalogued in `crates/framework/spot-errors/catalogue/sealed_fields.toml`.

use spot_protected_keys::{GroupName, NodeId};
use thiserror::Error;

/// The group is tagged `secret`, `pii` or `financial` and the tenant never chose a sync class.
pub const CLASS_UNSET: &str = "sealed_fields.class_unset";
/// A stored sync class is not one of the four names.
pub const CLASS_INVALID: &str = "sealed_fields.class_invalid";
/// A settings document or value is not in the expected shape.
pub const SETTING_INVALID: &str = "sealed_fields.setting_invalid";
/// The protection group is not declared.
pub const UNKNOWN_GROUP: &str = "sealed_fields.unknown_group";
/// A node tried to read a sealed value back; a node holds no key that could open it.
pub const SEALED_UNREADABLE: &str = "sealed_fields.sealed_unreadable";
/// A sealed value or a ciphertext failed authentication.
pub const TAMPERED: &str = "sealed_fields.tampered";
/// A sealed value is not in the expected shape.
pub const MALFORMED: &str = "sealed_fields.malformed";
/// The group key lease has passed and the read fails closed.
pub const LEASE_EXPIRED: &str = "sealed_fields.lease_expired";
/// The node is revoked and may hold no key and reveal nothing.
pub const NODE_REVOKED: &str = "sealed_fields.node_revoked";
/// The hub could not be reached or refused the call.
pub const HUB_UNAVAILABLE: &str = "sealed_fields.hub_unavailable";
/// A home-only field was asked for at a branch.
pub const HOME_ONLY: &str = "sealed_fields.home_only";
/// A channel has no guard registration.
pub const CHANNEL_UNREGISTERED: &str = "sealed_fields.channel_unregistered";

/// A failure of the sync-class, sealed-field or channel layer. Messages never carry a value.
#[derive(Debug, Error)]
pub enum SealedFieldsError {
    /// A tagged group has no explicit sync class.
    #[error(
        "sealed_fields.class_unset: protection group {group} has no sync class and its tags have no default"
    )]
    ClassUnset {
        /// The protection group.
        group: GroupName,
    },
    /// A stored sync class name is not one of the four.
    #[error("sealed_fields.class_invalid: protection group {group} names sync class `{value}`")]
    ClassInvalid {
        /// The protection group as the document spelled it.
        group: String,
        /// The unknown class name.
        value: String,
    },
    /// A settings document or value is malformed.
    #[error("sealed_fields.setting_invalid: {detail}")]
    SettingInvalid {
        /// What is wrong, without a value.
        detail: String,
    },
    /// The protection group is not declared.
    #[error("sealed_fields.unknown_group: no protection group {group}")]
    UnknownGroup {
        /// The protection group.
        group: GroupName,
    },
    /// A node tried to read a sealed value back.
    #[error(
        "sealed_fields.sealed_unreadable: sealed field {field} can be written here and never read here"
    )]
    SealedUnreadable {
        /// The field path.
        field: String,
    },
    /// Authentication failed.
    #[error("sealed_fields.tampered: authentication failed")]
    Tampered,
    /// A value is not in the expected shape.
    #[error("sealed_fields.malformed: {0}")]
    Malformed(&'static str),
    /// The lease has passed.
    #[error("sealed_fields.lease_expired: the key lease for group {group} has passed")]
    LeaseExpired {
        /// The protection group.
        group: GroupName,
    },
    /// The node is revoked.
    #[error("sealed_fields.node_revoked: node {node} is revoked")]
    NodeRevoked {
        /// The node id.
        node: NodeId,
    },
    /// The hub could not be reached.
    #[error("sealed_fields.hub_unavailable: the hub did not answer")]
    HubUnavailable,
    /// A home-only field was asked for at a branch.
    #[error("sealed_fields.home_only: group {group} does not sync to a branch")]
    HomeOnly {
        /// The protection group.
        group: GroupName,
    },
    /// A channel has no guard registration.
    #[error("sealed_fields.channel_unregistered: channel {channel} has no guard registration")]
    ChannelUnregistered {
        /// The channel's stable name.
        channel: String,
    },
}

impl SealedFieldsError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ClassUnset { .. } => CLASS_UNSET,
            Self::ClassInvalid { .. } => CLASS_INVALID,
            Self::SettingInvalid { .. } => SETTING_INVALID,
            Self::UnknownGroup { .. } => UNKNOWN_GROUP,
            Self::SealedUnreadable { .. } => SEALED_UNREADABLE,
            Self::Tampered => TAMPERED,
            Self::Malformed(_) => MALFORMED,
            Self::LeaseExpired { .. } => LEASE_EXPIRED,
            Self::NodeRevoked { .. } => NODE_REVOKED,
            Self::HubUnavailable => HUB_UNAVAILABLE,
            Self::HomeOnly { .. } => HOME_ONLY,
            Self::ChannelUnregistered { .. } => CHANNEL_UNREGISTERED,
        }
    }
}

/// A refused reveal. Separate from [`SealedFieldsError`] because the reveal API has its own
/// user-facing answers (an offline screen says *why*, not that something broke).
#[derive(Debug, Error)]
pub enum RevealError {
    /// The principal lacks the field's `reveal` right (or the document does not exist).
    #[error("sealed_fields.reveal_denied: the field cannot be revealed")]
    Denied,
    /// No live hub call: reveal is online-only (ADR-070 rule 12.13 f).
    #[error("sealed_fields.reveal_offline: reveal needs a live connection to the hub")]
    Offline {
        /// The user-facing explanation key the screen shows.
        explanation_key: &'static str,
    },
    /// The node is revoked.
    #[error("sealed_fields.node_revoked: node {node} is revoked")]
    NodeRevoked {
        /// The node id.
        node: NodeId,
    },
    /// A person asked for a bulk reveal.
    #[error("sealed_fields.reveal_bulk_denied: bulk reveal is for service principals only")]
    BulkDenied,
    /// The service principal's manifest does not declare the group.
    #[error(
        "sealed_fields.group_undeclared: group {group} is not declared in the service manifest"
    )]
    GroupUndeclared {
        /// The protection group.
        group: GroupName,
    },
    /// The audit record could not be written, so nothing was released.
    #[error("sealed_fields.audit_unavailable: the reveal could not be audited")]
    AuditUnavailable,
    /// The hub refused or failed.
    #[error("sealed_fields.hub_unavailable: the hub did not answer")]
    HubUnavailable,
}

impl RevealError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied => REVEAL_DENIED,
            Self::Offline { .. } => REVEAL_OFFLINE,
            Self::NodeRevoked { .. } => NODE_REVOKED,
            Self::BulkDenied => REVEAL_BULK_DENIED,
            Self::GroupUndeclared { .. } => GROUP_UNDECLARED,
            Self::AuditUnavailable => AUDIT_UNAVAILABLE,
            Self::HubUnavailable => HUB_UNAVAILABLE,
        }
    }

    /// The user-facing explanation key for an offline reveal.
    #[must_use]
    pub fn offline(explanation_key: &'static str) -> Self {
        Self::Offline { explanation_key }
    }
}

/// The `reveal` right was absent.
pub const REVEAL_DENIED: &str = "sealed_fields.reveal_denied";
/// Reveal needs a live hub call.
pub const REVEAL_OFFLINE: &str = "sealed_fields.reveal_offline";
/// Bulk reveal is for service principals.
pub const REVEAL_BULK_DENIED: &str = "sealed_fields.reveal_bulk_denied";
/// The service manifest does not declare the group.
pub const GROUP_UNDECLARED: &str = "sealed_fields.group_undeclared";
/// The audit record could not be written.
pub const AUDIT_UNAVAILABLE: &str = "sealed_fields.audit_unavailable";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn group() -> GroupName {
        GroupName::new("bank_details").expect("group")
    }

    fn node() -> NodeId {
        NodeId::new("branch-1").expect("node")
    }

    fn probe() -> Vec<SealedFieldsError> {
        vec![
            SealedFieldsError::ClassUnset { group: group() },
            SealedFieldsError::ClassInvalid {
                group: "bank_details".to_owned(),
                value: "sometimes".to_owned(),
            },
            SealedFieldsError::SettingInvalid {
                detail: "the table is not `group=class` lines".to_owned(),
            },
            SealedFieldsError::UnknownGroup { group: group() },
            SealedFieldsError::SealedUnreadable {
                field: "salary".to_owned(),
            },
            SealedFieldsError::Tampered,
            SealedFieldsError::Malformed("truncated"),
            SealedFieldsError::LeaseExpired { group: group() },
            SealedFieldsError::NodeRevoked { node: node() },
            SealedFieldsError::HubUnavailable,
            SealedFieldsError::HomeOnly { group: group() },
            SealedFieldsError::ChannelUnregistered {
                channel: "journal".to_owned(),
            },
        ]
    }

    fn reveal_probe() -> Vec<RevealError> {
        vec![
            RevealError::Denied,
            RevealError::offline("error.sealed_fields.reveal_offline"),
            RevealError::NodeRevoked { node: node() },
            RevealError::BulkDenied,
            RevealError::GroupUndeclared { group: group() },
            RevealError::AuditUnavailable,
            RevealError::HubUnavailable,
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
            assert!(code.starts_with("sealed_fields."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        // `node_revoked` and `hub_unavailable` are deliberately the same codes on both types:
        // the condition is one thing to a caller, whichever door it came through.
        let shared = [NODE_REVOKED, HUB_UNAVAILABLE];
        let mut reveal_seen = std::collections::BTreeSet::new();
        for error in reveal_probe() {
            let code = error.code();
            assert!(code.starts_with("sealed_fields."), "{code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
            if shared.contains(&code) {
                assert!(
                    seen.contains(code),
                    "{code} is missing from the error probe"
                );
            } else {
                assert!(seen.insert(code), "duplicate code {code}");
            }
            reveal_seen.insert(code);
        }
        assert_eq!(seen.len(), 17, "a variant is missing from the probe");
        assert_eq!(reveal_seen.len(), 7, "a reveal variant is missing");
    }

    #[test]
    fn messages_carry_no_value() {
        for error in probe() {
            let text = error.to_string();
            assert!(text.contains(error.code()), "{text}");
            assert!(!text.contains("value="), "{text}");
        }
        for error in reveal_probe() {
            let text = error.to_string();
            assert!(text.contains(error.code()), "{text}");
        }
    }
}
