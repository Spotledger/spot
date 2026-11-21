//! The operation ids this crate owns, in one place so the catalogue, the dispatcher and the
//! tests agree and a typo is a compile error.
//!
//! Spelled with [`concat!`] rather than as dotted literals: operation ids are not error codes,
//! and the repository's error-code lint reads every dotted lower-case literal as one (the same
//! rule the Linux and database modules follow).

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

/// The journal's status: position, durability state and lag.
pub const STATUS: &str = op2!(journal, status);
/// The recovery report: the answer to "could I recover?" (ADR-061 section 8).
pub const RECOVERY_REPORT: &str = op3!(journal, recovery, report);
/// List snapshots, newest first.
pub const SNAPSHOT_LIST: &str = op3!(journal, snapshot, list);
/// Take a snapshot now.
pub const SNAPSHOT_TAKE: &str = op3!(journal, snapshot, take);
/// Verify the journal's chain and segments.
pub const VERIFY: &str = op2!(journal, verify);
/// The last journal audit's result.
pub const AUDIT_LAST: &str = op3!(journal, audit, last);
/// Run the journal audit now.
pub const AUDIT_RUN: &str = op3!(journal, audit, run);
/// Read the durability policy.
pub const POLICY_GET: &str = op3!(journal, policy, get);
/// Set the durability policy.
pub const POLICY_SET: &str = op3!(journal, policy, set);
/// Re-ship journal segments that are not yet off site.
pub const RESHIP: &str = op2!(journal, reship);
/// Plan a rebuild (read only).
pub const REBUILD_PLAN: &str = op3!(journal, rebuild, plan);
/// Run a rebuild whose plan a person has seen.
pub const REBUILD_RUN: &str = op3!(journal, rebuild, run);
/// List the journal's reader grants.
pub const READER_LIST: &str = op3!(journal, reader, list);
/// Grant a reader access.
pub const READER_GRANT: &str = op3!(journal, reader, grant);
/// Revoke a reader's access.
pub const READER_REVOKE: &str = op3!(journal, reader, revoke);
/// Erase one data subject's personal fields (destroys the per-subject key).
pub const SUBJECT_ERASE: &str = op3!(journal, subject, erase);

/// Every id this crate registers, in catalogue order.
pub const ALL: [&str; 16] = [
    STATUS,
    RECOVERY_REPORT,
    SNAPSHOT_LIST,
    SNAPSHOT_TAKE,
    VERIFY,
    AUDIT_LAST,
    AUDIT_RUN,
    POLICY_GET,
    POLICY_SET,
    RESHIP,
    REBUILD_PLAN,
    REBUILD_RUN,
    READER_LIST,
    READER_GRANT,
    READER_REVOKE,
    SUBJECT_ERASE,
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
        assert_eq!(seen.len(), 16);
    }
}
