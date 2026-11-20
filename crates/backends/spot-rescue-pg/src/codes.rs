//! The stable `rescue_pg.*` error codes (catalogue `rescue_pg.toml`).

/// The control schema, its table or a row could not be read or written.
pub const CONTROL_UNAVAILABLE: &str = "rescue_pg.control_unavailable";
/// The journal rejected a sequence number that is not the next one (a gap, or a fork).
pub const JOURNAL_SEQUENCE: &str = "rescue_pg.journal_sequence";
/// A stored journal record does not parse back into a record.
pub const JOURNAL_RECORD: &str = "rescue_pg.journal_record";
