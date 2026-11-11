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
