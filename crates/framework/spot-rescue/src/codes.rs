//! The stable `rescue.*` error codes of the rescue tool.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/rescue.toml` with its class, a
//! one-sentence summary and a remedy. The codes carry the `rescue.` prefix because the
//! catalogue requires a file's name to be its entries' prefix.

/// The run has no two-person `restore` ticket.
pub const NO_TICKET: &str = "rescue.no_ticket";
/// The `restore` ticket does not authorise this snapshot time.
pub const TICKET_MISMATCH: &str = "rescue.ticket_mismatch";
/// The `restore` ticket does not name two distinct approvers.
pub const NOT_TWO_PERSON: &str = "rescue.not_two_person";
/// The run names no taken snapshot.
pub const NO_SNAPSHOT: &str = "rescue.no_snapshot";
/// The snapshot could not be checked or restored.
pub const SNAPSHOT_FAILED: &str = "rescue.snapshot_failed";
/// The maintenance gate could not be closed or opened.
pub const GATE_FAILED: &str = "rescue.gate_failed";
/// The rescue journal could not be written or read.
pub const JOURNAL_FAILED: &str = "rescue.journal_failed";
/// At least one applied patch has no declared down-step, so no down-patch plan exists.
pub const IRREVERSIBLE: &str = "rescue.irreversible";
/// An argument is not in its documented form.
pub const INVALID_INPUT: &str = "rescue.invalid_input";
/// The run was killed at a fault-injection boundary; nothing was cleaned up.
pub const KILLED: &str = "rescue.killed";
/// A re-entered item failed in the old version's engine; the run stopped and needs a person.
pub const REENTRY_FAILED: &str = "rescue.reentry_failed";
/// An operation cannot be expressed in the old version and blocks the run.
pub const BLOCKED: &str = "rescue.blocked";
/// The final invariant pass failed; users stay out.
pub const INVARIANTS_FAILED: &str = "rescue.invariants_failed";
/// A step of a down-patch plan failed; the site stays closed.
pub const DOWN_STEP_FAILED: &str = "rescue.down_step_failed";
/// The journal belongs to a different rescue identity or snapshot.
pub const RUN_MISMATCH: &str = "rescue.run_mismatch";
/// The operation names a DocType the old version does not have.
pub const UNKNOWN_DOCTYPE: &str = "rescue.unknown_doctype";
/// The operation carries a field the old version does not have and no transform removes it.
pub const UNKNOWN_FIELD: &str = "rescue.unknown_field";
/// The operation lacks a field the old version requires and no transform supplies it.
pub const MISSING_FIELD: &str = "rescue.missing_field";
/// The operation kind (an update or delete of something the snapshot no longer has) cannot be
/// expressed against the restored old version.
pub const UNSUPPORTED_OPERATION: &str = "rescue.unsupported_operation";
/// Two or more operations share one id, so causal order is ambiguous.
pub const DUPLICATE_OPERATION: &str = "rescue.duplicate_operation";
/// The causal links between operations form a cycle.
pub const CAUSAL_CYCLE: &str = "rescue.causal_cycle";
