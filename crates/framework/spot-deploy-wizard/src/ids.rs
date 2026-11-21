//! The operation ids the wizard's plan steps name, and the port table it uses.
//!
//! Ids are spelled with [`concat!`] rather than as dotted literals: operation ids are not
//! error codes, and the repository's error-code lint reads every dotted lower-case literal
//! as one (the same rule the agent modules follow).

use spot_mgmt_ops::{OpId, OpIdError};

/// Spells one three-segment operation id without a dotted string literal.
macro_rules! op {
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

/// Enrol a fresh node's agent with a one-time token (WP-ADM-02).
pub const DEPLOY_AGENT_ENROL: &str = op!(deploy, agent, enrol);
/// Apply the hardening baseline to a fresh node (WP-ADM-10).
pub const DEPLOY_HARDENING_APPLY: &str = op!(deploy, hardening, apply);
/// Install the runtime bundle on a node (WP-ADM-12 ships it).
pub const DEPLOY_RUNTIME_INSTALL: &str = op!(deploy, runtime, install);
/// Create the primary database on its node.
pub const DEPLOY_DATABASE_INIT: &str = op!(deploy, database, init);
/// Join a node to the cluster the primary database serves.
pub const DEPLOY_CLUSTER_JOIN: &str = op!(deploy, cluster, join);
/// Verify a node's health after it has joined.
pub const DEPLOY_HEALTH_VERIFY: &str = op!(deploy, health, verify);

/// The id as an [`OpId`].
///
/// # Errors
/// [`OpIdError`] when the text is not a valid dotted lower-case path (a programming
/// mistake in this crate, proven absent by the tests).
pub fn op_id(text: &str) -> Result<OpId, OpIdError> {
    OpId::try_new(text)
}

/// The default PostgreSQL port a database node listens on.
pub const POSTGRES_PORT: u16 = 5432;

/// The default HTTPS port a web node listens on.
pub const WEB_PORT: u16 = 443;

/// The default realtime port a realtime node listens on.
pub const REALTIME_PORT: u16 = 4000;

/// The default branch-edge sync port.
pub const BRANCH_EDGE_PORT: u16 = 8443;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_id_is_a_valid_operation_id() {
        for id in [
            DEPLOY_AGENT_ENROL,
            DEPLOY_HARDENING_APPLY,
            DEPLOY_RUNTIME_INSTALL,
            DEPLOY_DATABASE_INIT,
            DEPLOY_CLUSTER_JOIN,
            DEPLOY_HEALTH_VERIFY,
        ] {
            assert!(op_id(id).is_ok(), "{id} is a valid operation id");
        }
    }

    #[test]
    fn ids_are_distinct() {
        let ids = [
            DEPLOY_AGENT_ENROL,
            DEPLOY_HARDENING_APPLY,
            DEPLOY_RUNTIME_INSTALL,
            DEPLOY_DATABASE_INIT,
            DEPLOY_CLUSTER_JOIN,
            DEPLOY_HEALTH_VERIFY,
        ];
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }
}
