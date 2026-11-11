//! The operation ids this crate owns.
//!
//! Kept in one place so the catalogue, the handlers and the tests agree, and so a typo is a
//! compile error rather than a missing handler at runtime.
//!
//! The ids are spelled with [`concat!`] rather than as dotted literals: operation ids are not
//! error codes, and the repository's error-code lint reads every dotted lower-case literal as
//! one (the same rule the Linux modules follow).

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

/// Read one allow-listed PostgreSQL parameter.
pub const DB_PARAM_GET: &str = op!(db, param, get);
/// Set one allow-listed PostgreSQL parameter through `ALTER SYSTEM`.
pub const DB_PARAM_SET: &str = op!(db, param, set);
/// Read replication status.
pub const DB_REPLICA_STATUS: &str = op!(db, replica, status);
/// Read replication lag.
pub const DB_REPLICA_LAG: &str = op!(db, replica, lag);
/// Create a standby with `pg_basebackup`.
pub const DB_REPLICA_CREATE: &str = op!(db, replica, create);
/// Promote a standby, guarded against a reachable primary.
pub const DB_REPLICA_PROMOTE: &str = op!(db, replica, promote);
/// Start a base backup or a logical dump.
pub const DB_BACKUP_START: &str = op!(db, backup, start);
/// List the backups on this node.
pub const DB_BACKUP_LIST: &str = op!(db, backup, list);
/// Verify one backup's integrity.
pub const DB_BACKUP_VERIFY: &str = op!(db, backup, verify);
/// Read a node's role set.
pub const NODE_ROLE_GET: &str = op!(node, role, get);
/// Replace a node's role set.
pub const NODE_ROLE_SET: &str = op!(node, role, set);
/// Apply a named runtime profile.
pub const RUNTIME_PROFILE_APPLY: &str = op!(runtime, profile, apply);
/// Start an upgrade through the migration lifecycle.
pub const RUNTIME_UPGRADE_START: &str = op!(runtime, upgrade, start);
/// Resume an interrupted upgrade from the engine's journal.
pub const RUNTIME_UPGRADE_RESUME: &str = op!(runtime, upgrade, resume);
/// Stop the runtime service with a drain.
pub const RUNTIME_SERVICE_STOP: &str = op!(runtime, service, stop);
/// Start the runtime service.
pub const RUNTIME_SERVICE_START: &str = op!(runtime, service, start);

/// The id as an [`OpId`].
///
/// # Errors
/// [`OpIdError`] when the constant is not a valid dotted lower-case path (a programming
/// mistake in this crate, proven absent by the tests).
pub fn op_id(text: &str) -> Result<OpId, OpIdError> {
    OpId::try_new(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_id_is_valid() {
        for id in [
            DB_PARAM_GET,
            DB_PARAM_SET,
            DB_REPLICA_STATUS,
            DB_REPLICA_LAG,
            DB_REPLICA_CREATE,
            DB_REPLICA_PROMOTE,
            DB_BACKUP_START,
            DB_BACKUP_LIST,
            DB_BACKUP_VERIFY,
            NODE_ROLE_GET,
            NODE_ROLE_SET,
            RUNTIME_PROFILE_APPLY,
            RUNTIME_UPGRADE_START,
            RUNTIME_UPGRADE_RESUME,
            RUNTIME_SERVICE_STOP,
            RUNTIME_SERVICE_START,
        ] {
            assert!(op_id(id).is_ok(), "{id} is a valid operation id");
        }
    }
}
