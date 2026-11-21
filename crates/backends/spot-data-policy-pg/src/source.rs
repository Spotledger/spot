//! The record half of the store: live records and the effect performed on them.
//!
//! The ledger lives in PostgreSQL; the records belong to the document store. A host implements
//! [`RecordSource`] over the document lifecycle (delete, archive) and the anonymise operation
//! (ADR-051). This crate never edits a record with SQL.

use spot_data_policy::store::ApplyItem;
use spot_data_policy::{DataPolicyError, Record};

/// Live records and the effects retention applies to them.
///
/// `apply_effect` runs while the ledger's transaction is open and not yet committed, on a
/// blocking thread. If it returns an error the ledger rows are rolled back and the item can be
/// retried. If the process dies, or the commit fails, **after** `apply_effect` returned, the
/// effect is done but unrecorded and a retry calls `apply_effect` again: an implementation must
/// therefore treat a repeated effect on an already-deleted, already-archived or
/// already-anonymised record as success (the engine's own `already_gone` / `already_done` skip
/// rules cover the planning side, this covers the crash window). It must not read the wall clock;
/// the item carries the instant.
pub trait RecordSource: Send + 'static {
    /// Every live record, in stable id order.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the records cannot be listed. A failed listing is
    /// never an empty one: a plan built from "no records" would retain everything.
    fn records(&self) -> Result<Vec<Record>, DataPolicyError>;

    /// One live record, `Ok(None)` when it is not there.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the read fails (which is not "the record is gone").
    fn record(&self, id: &str) -> Result<Option<Record>, DataPolicyError>;

    /// Performs `item.effect` and writes `item.audit` (when present) through the document
    /// lifecycle. The outcome and the deletion proof are the ledger's, not the source's.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the effect cannot be applied.
    fn apply_effect(&mut self, item: &ApplyItem) -> Result<(), DataPolicyError>;
}
