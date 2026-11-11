//! The stable `rehearsal.*` error codes.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/rehearsal.toml` with its class, a
//! one-sentence summary and a remedy. The codes carry the `rehearsal.` prefix because the
//! catalogue requires a file's name to be its entries' prefix.

/// The approved ticket's plan digest does not match the plan being rehearsed.
pub const TICKET_MISMATCH: &str = "rehearsal.ticket_mismatch";
/// An identifier, a spec or a query is not in its documented form.
pub const INVALID_INPUT: &str = "rehearsal.invalid_input";
/// Taking the snapshot or restoring it into the scratch database failed.
pub const SNAPSHOT_FAILED: &str = "rehearsal.snapshot_failed";
/// Applying the plan to the clone failed.
pub const APPLY_FAILED: &str = "rehearsal.apply_failed";
/// A fingerprint query failed, or a measure it returned was not an integer.
pub const FINGERPRINT_FAILED: &str = "rehearsal.fingerprint_failed";
