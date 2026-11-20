//! The stable `serverscript.*` codes this crate raises.
//!
//! Every code lives in `crates/framework/spot-errors/catalogue/serverscript.toml`, which the
//! error-code completeness scan checks in both directions. The codes here are the crate's own;
//! the engine's `script.*` codes (`script.timeout`, `script.validation`, ...) stay the engine's
//! and are mapped, not re-declared.

/// The TypeScript source does not compile, or the engine refuses it. Raised on save as a field
/// error on the source field and on load when a stored script cannot be prepared.
pub const COMPILE_FAILED: &str = "serverscript.compile_failed";

/// A script exceeded one of its limits (time, memory, instructions, host calls or output size).
/// The operation fails and the document is not saved.
pub const LIMIT_EXCEEDED: &str = "serverscript.limit_exceeded";

/// A script did something its phase does not allow, for example a `validate` script that
/// returned a patch to its own document.
pub const PHASE_VIOLATION: &str = "serverscript.phase_violation";

/// A `Server Script` record is not usable: a missing or malformed name, DocType, event or
/// source.
pub const RECORD_INVALID: &str = "serverscript.record_invalid";

/// The script store could not load or record what it was asked to.
pub const STORE_FAILED: &str = "serverscript.store_failed";

/// An API script was called without a signed-in user (a guest, or a caller with no usable
/// identity). API scripts run as their caller and are never guest-callable.
pub const GUEST_REFUSED: &str = "serverscript.guest_refused";

/// A scheduled script's `run_as` user is missing or disabled, so the run is refused. A
/// scheduled run never falls back to a system principal.
pub const RUN_AS_UNAVAILABLE: &str = "serverscript.run_as_unavailable";

/// Two API scripts claim one function key.
pub const FUNCTION_CONFLICT: &str = "serverscript.function_conflict";

/// A scheduled script's cron expression or zone cannot be scheduled.
pub const SCHEDULE_INVALID: &str = "serverscript.schedule_invalid";

/// A save turned a script's `elevated` flag on, or edited an elevated script, without holding
/// the `scripts_elevate` permission.
pub const ELEVATION_FORBIDDEN: &str = "serverscript.elevation_forbidden";

/// An elevated script needs a reason of 20 to 500 characters.
pub const ELEVATION_REASON_REQUIRED: &str = "serverscript.elevation_reason_required";

/// The warning shown when a save of changed source took the elevation off a script.
pub const ELEVATION_RESET: &str = "serverscript.elevation_reset";

/// A kind of script that can never be elevated (a scheduled script) carries the flag.
pub const ELEVATION_NOT_ALLOWED: &str = "serverscript.elevation_not_allowed";

/// An elevated run could not be audited, so it was refused or failed (no audit, no elevated
/// write).
pub const ELEVATION_AUDIT_FAILED: &str = "serverscript.elevation_audit_failed";
