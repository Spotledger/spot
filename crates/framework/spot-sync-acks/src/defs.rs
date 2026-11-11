//! The condition definitions the ledger raises, taken from the `spot-sync-conditions` catalogue.

use spot_conditions::ConditionDef;

use crate::error::AckError;

/// Past half the delivery window: information only.
pub const OBLIGATION_DUE_SOON: &str = "OBLIGATION_DUE_SOON";
/// Past the deadline: the mandatory lane goes first and the node's contact is notified (the
/// `sync_admin` role is told at the red rung, ADR-065 section 3).
pub const OBLIGATION_OVERDUE: &str = "OBLIGATION_OVERDUE";
/// Past the grace, or an effective date is near: `sync_admin` is notified.
pub const OBLIGATION_RED: &str = "OBLIGATION_RED";

/// The three definitions the ledger needs, one per ladder level above ok, from the catalogue.
///
/// Each rung is its own condition on the subject `node:<id>`, raised when the node reaches the
/// rung and cleared when it falls below it.
pub(crate) fn catalogue_defs() -> Result<Vec<ConditionDef>, AckError> {
    [OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE, OBLIGATION_RED]
        .into_iter()
        .map(|id| {
            spot_sync_conditions::entry(id)
                .map(|e| e.def)
                .ok_or(AckError::ConditionMissing { id })
        })
        .collect()
}
