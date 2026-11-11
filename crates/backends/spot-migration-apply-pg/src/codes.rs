//! The stable `migration_apply_pg.*` error codes of the PostgreSQL wiring.
//!
//! Every code here has one entry in `spot-errors`'
//! `catalogue/migration_apply_pg.toml`, with its class, a one-sentence summary and a remedy.
//! The codes carry the `migration_apply_pg.` prefix because the catalogue requires a file's
//! name to be its entries' prefix.

/// The control schema, its tables or its rows could not be read or written.
pub const CONTROL_UNAVAILABLE: &str = "migration_apply_pg.control_unavailable";
/// The journal rejected a sequence number that is not the next one.
pub const JOURNAL_SEQUENCE: &str = "migration_apply_pg.journal_sequence";
/// A stored journal record does not parse back into a record.
pub const JOURNAL_RECORD: &str = "migration_apply_pg.journal_record";
/// The maintenance gate could not be read, so it is treated as closed.
pub const GATE_UNREADABLE: &str = "migration_apply_pg.gate_unreadable";
/// The step's index is outside the plan the runner was prepared for.
pub const STEP_OUT_OF_PLAN: &str = "migration_apply_pg.step_out_of_plan";
/// The schema step failed; the engine restores the snapshot.
pub const STEP_FAILED: &str = "migration_apply_pg.step_failed";
/// A non-schema step has no handler wired in this build.
pub const STEP_UNSUPPORTED: &str = "migration_apply_pg.step_unsupported";
/// The snapshot provider failed to take, verify or restore a snapshot.
pub const SNAPSHOT_FAILED: &str = "migration_apply_pg.snapshot_failed";
/// The snapshot id is not one this port took on this site.
pub const SNAPSHOT_UNKNOWN: &str = "migration_apply_pg.snapshot_unknown";
/// A timing measurement could not be recorded.
pub const TIMING_FAILED: &str = "migration_apply_pg.timing_failed";
/// A `migration.progress` notification could not be published.
pub const PROGRESS_FAILED: &str = "migration_apply_pg.progress_failed";
/// The room name or the tenant room could not be built.
pub const PROGRESS_ROOM: &str = "migration_apply_pg.progress_room";
