//! The stable `migration_apply.*` error codes of the apply engine.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/migration_apply.toml` with its
//! class, a one-sentence summary and a remedy. The codes carry the `migration_apply.` prefix
//! because the catalogue requires a file's name to be its entries' prefix.

/// The run has no `ApplyTicket`, so it may not start.
pub const NO_TICKET: &str = "migration_apply.no_ticket";
/// The run names no snapshot, or the snapshot it names was not taken.
pub const NO_SNAPSHOT: &str = "migration_apply.no_snapshot";
/// The ticket authorises a different plan than the one in hand.
pub const TICKET_MISMATCH: &str = "migration_apply.ticket_mismatch";
/// An earlier run of a different plan is still unfinished on this site.
pub const IN_PROGRESS: &str = "migration_apply.in_progress";
/// The plan was already applied and verified on this site; one ticket authorises one application.
pub const ALREADY_APPLIED: &str = "migration_apply.already_applied";
/// Writing or reading the apply journal failed; the run stops fail closed.
pub const JOURNAL_FAILED: &str = "migration_apply.journal_failed";
/// The maintenance gate could not be closed or opened.
pub const GATE_FAILED: &str = "migration_apply.gate_failed";
/// In-flight transactions did not drain within the deadline.
pub const DRAIN_TIMEOUT: &str = "migration_apply.drain_timeout";
/// The side-effect hold could not be begun.
pub const HOLD_FAILED: &str = "migration_apply.hold_failed";
/// A step failed; the engine restores the snapshot.
pub const STEP_FAILED: &str = "migration_apply.step_failed";
/// A step exceeded its timeout.
pub const STEP_TIMEOUT: &str = "migration_apply.step_timeout";
/// Verification failed; the engine restores the snapshot.
pub const VERIFY_FAILED: &str = "migration_apply.verify_failed";
/// Compensating or restoring a step failed; the site stays closed.
pub const RESTORE_FAILED: &str = "migration_apply.restore_failed";
/// Restoring from the snapshot failed; the site stays closed.
pub const SNAPSHOT_FAILED: &str = "migration_apply.snapshot_failed";
/// Releasing the held effects after verification failed; the site stays closed.
pub const RELEASE_FAILED: &str = "migration_apply.release_failed";
/// The run halted and needs a person; the site stays closed.
pub const HALTED: &str = "migration_apply.halted";
/// An argument is not in its documented form.
pub const INVALID_INPUT: &str = "migration_apply.invalid_input";
/// The run was killed (a simulated crash in a fault-injection test); nothing is cleaned up.
pub const KILLED: &str = "migration_apply.killed";
/// The journal says the new version is open but not verified.
pub const OPEN_UNVERIFIED: &str = "migration_apply.open_unverified";
