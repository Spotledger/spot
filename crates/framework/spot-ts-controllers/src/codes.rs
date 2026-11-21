//! The stable `tscontroller.*` codes this crate raises.
//!
//! Every code lives in `crates/framework/spot-errors/catalogue/tscontroller.toml`, which the
//! error-code completeness scan checks in both directions. The engine's `script.*` codes
//! (`script.timeout`, `script.thrown`, ...) and the native-controller `whitelist.bad_arguments`
//! stay where they are declared and are mapped, not re-declared.

/// The module's TypeScript does not compile, or the engine refuses the compiled module. Raised
/// when an app installs or upgrades and the controller module cannot be prepared.
pub const COMPILE_FAILED: &str = "tscontroller.compile_failed";

/// The module's `contract()` export does not match the module contract: every problem is
/// reported at once, each with the position of the declaration that is wrong.
pub const INVALID_MODULE: &str = "tscontroller.invalid_module";

/// A step or method exceeded one of its limits (time, memory, instructions, host calls or output
/// size). The operation fails and the transaction is rolled back.
pub const LIMIT_EXCEEDED: &str = "tscontroller.limit_exceeded";

/// A method handler returned a result that does not match its declared result schema. The result
/// is refused and the method's transaction is rolled back.
pub const RESULT_INVALID: &str = "tscontroller.result_invalid";

/// The `ts:<path>` reference names a module the installed app bundle does not carry.
pub const MODULE_MISSING: &str = "tscontroller.module_missing";
