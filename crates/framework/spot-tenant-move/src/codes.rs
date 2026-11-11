//! Stable `tenant_move.*` error codes (catalogued in `spot-errors/catalogue/tenant_move.toml`).

/// The move plan is malformed (same source and target, a schema not derived from the id).
pub const INVALID_PLAN: &str = "tenant_move.invalid_plan";
/// The move id is malformed.
pub const INVALID_MOVE_ID: &str = "tenant_move.invalid_move_id";
/// No move is recorded for the tenant.
pub const NOT_FOUND: &str = "tenant_move.not_found";
/// The tenant already has a move record.
pub const ALREADY_EXISTS: &str = "tenant_move.already_exists";
/// The state machine does not allow the operation in the current state.
pub const WRONG_STATE: &str = "tenant_move.wrong_state";
/// The residency policy forbids the target region.
pub const RESIDENCY_REFUSED: &str = "tenant_move.residency_refused";
/// The admission gate or the side-effect hold could not be engaged or released.
pub const QUIESCE_FAILED: &str = "tenant_move.quiesce_failed";
/// The transfer port failed.
pub const TRANSFER_FAILED: &str = "tenant_move.transfer_failed";
/// A chunk's bytes do not match its checksum.
pub const CHUNK_CORRUPT: &str = "tenant_move.chunk_corrupt";
/// The move store failed or its optimistic revision did not match.
pub const STORE_FAILED: &str = "tenant_move.store_failed";
/// A fingerprint could not be measured.
pub const FINGERPRINT_FAILED: &str = "tenant_move.fingerprint_failed";
/// The source and target fingerprints differ, so cut-over is refused.
pub const FINGERPRINT_MISMATCH: &str = "tenant_move.fingerprint_mismatch";
/// The placement flip or cache invalidation failed.
pub const CUTOVER_FAILED: &str = "tenant_move.cutover_failed";
/// The move has already cut over and cannot be aborted.
pub const ABORT_TOO_LATE: &str = "tenant_move.abort_too_late";
/// A step returned without advancing the state machine.
pub const STEP_FAILED: &str = "tenant_move.step_failed";
