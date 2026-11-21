//! The operation ids this crate owns, in one place so the catalogue, the dispatcher and the
//! tests agree and a typo is a compile error.
//!
//! Spelled with [`concat!`] rather than as dotted literals: operation ids are not error codes,
//! and the repository's error-code lint reads every dotted lower-case literal as one (the same
//! rule the Linux, database and journal modules follow).

use spot_mgmt_ops::{OpId, OpIdError};

/// Spells a two-segment operation id without a dotted string literal.
macro_rules! op2 {
    ($group:ident, $action:ident) => {
        concat!(stringify!($group), ".", stringify!($action))
    };
}

/// Spells a three-segment operation id without a dotted string literal.
macro_rules! op3 {
    ($group:ident, $sub:ident, $action:ident) => {
        concat!(
            stringify!($group),
            ".",
            stringify!($sub),
            ".",
            stringify!($action)
        )
    };
}

/// The sync health view: calm status, the needs-you list and per-node rows.
pub const HEALTH: &str = op2!(sync, health);
/// List the tenant's sync nodes.
pub const NODE_LIST: &str = op3!(sync, node, list);
/// Show one sync node.
pub const NODE_SHOW: &str = op3!(sync, node, show);
/// Enrol a node: issues a one-time enrolment code, returned once.
pub const NODE_ENROL: &str = op3!(sync, node, enrol);
/// Suspend a node: it stops syncing until resumed.
pub const NODE_SUSPEND: &str = op3!(sync, node, suspend);
/// Resume a suspended node.
pub const NODE_RESUME: &str = op3!(sync, node, resume);
/// Revoke a node's membership for good.
pub const NODE_REVOKE: &str = op3!(sync, node, revoke);
/// Rotate a node's certificate.
pub const NODE_ROTATE_CERTIFICATE: &str = op3!(sync, node, rotate_certificate);
/// Resend the stream to a node from a position.
pub const NODE_RESEND: &str = op3!(sync, node, resend);
/// Notify the node's contact person.
pub const NODE_NOTIFY_CONTACT: &str = op3!(sync, node, notify_contact);
/// List the open sync conditions.
pub const CONDITION_LIST: &str = op3!(sync, condition, list);
/// List the quarantined items.
pub const QUARANTINE_LIST: &str = op3!(sync, quarantine, list);
/// Retry applying a quarantined item.
pub const QUARANTINE_RETRY: &str = op3!(sync, quarantine, retry);
/// Discard a quarantined item.
pub const QUARANTINE_DISCARD: &str = op3!(sync, quarantine, discard);
/// Show the sync policy of a doctype.
pub const POLICY_SHOW: &str = op3!(sync, policy, show);
/// Lint the sync policies.
pub const POLICY_LINT: &str = op3!(sync, policy, lint);
/// Change this site's sync role.
pub const ROLE_CHANGE: &str = op3!(sync, role, change);
/// Read the bootstrap progress.
pub const BOOTSTRAP_STATUS: &str = op3!(sync, bootstrap, status);

/// Every id this crate registers, in catalogue order.
pub const ALL: [&str; 18] = [
    HEALTH,
    NODE_LIST,
    NODE_SHOW,
    NODE_ENROL,
    NODE_SUSPEND,
    NODE_RESUME,
    NODE_REVOKE,
    NODE_ROTATE_CERTIFICATE,
    NODE_RESEND,
    NODE_NOTIFY_CONTACT,
    CONDITION_LIST,
    QUARANTINE_LIST,
    QUARANTINE_RETRY,
    QUARANTINE_DISCARD,
    POLICY_SHOW,
    POLICY_LINT,
    ROLE_CHANGE,
    BOOTSTRAP_STATUS,
];

/// The id as an [`OpId`].
///
/// # Errors
/// [`OpIdError`] when the text is not a valid dotted lower-case path.
pub fn op_id(text: &str) -> Result<OpId, OpIdError> {
    OpId::try_new(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_is_valid_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for id in ALL {
            assert!(op_id(id).is_ok(), "{id} is a valid operation id");
            assert!(seen.insert(id), "{id} is listed once");
        }
        assert_eq!(seen.len(), 18);
    }
}
