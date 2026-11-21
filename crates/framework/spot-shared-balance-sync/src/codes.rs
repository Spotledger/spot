//! The stable `shared_balance_sync.*` codes of the sync-layer wiring.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/shared_balance_sync.toml`. Like
//! the `shared_balance.*` vocabulary, these are **library-internal** (class `none`): a refused
//! batch is an answer to a sender, not an `AppError`, and nothing here rides one.

/// The transport authenticated one node, and the batch named another.
pub const SENDER_MISMATCH: &str = "shared_balance_sync.sender_mismatch";
/// The node that sent the batch is not in the hub's registry.
pub const NODE_NOT_REGISTERED: &str = "shared_balance_sync.node_not_registered";
/// The node that sent the batch is registered but suspended or decommissioned.
pub const NODE_NOT_ACTIVE: &str = "shared_balance_sync.node_not_active";
/// The audit of a refused batch could not be written, so the refusal stands but is unrecorded.
pub const AUDIT_FAILED: &str = "shared_balance_sync.audit_failed";
/// The quarantine queue could not take a refused entry, so the batch is not acknowledged.
pub const QUARANTINE_FAILED: &str = "shared_balance_sync.quarantine_failed";
/// The audit event a rejected batch is recorded as.
pub const BATCH_REJECTED: &str = "shared_balance_sync.batch_rejected";
