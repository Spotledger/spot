//! The `controller.*` error codes this crate raises.
//!
//! Every code here has one `[[entry]]` in `crates/framework/spot-errors/catalogue/controller.toml`
//! (ADR-26). A code raised anywhere in the repository without an entry fails the error-code
//! completeness scan, and an entry nothing raises fails it too, so the two stay in step.

/// Two apps declare the same whitelisted method on one DocType, and neither overrides the other.
pub const METHOD_CONFLICT: &str = "controller.method_conflict";

/// No host adapter can load this kind of controller yet.
pub const HOST_UNAVAILABLE: &str = "controller.host_unavailable";

/// A controller reference is not one of `native:<name>`, `wasm:<export>` or `ts:<path>`.
pub const INVALID_REFERENCE: &str = "controller.invalid_reference";

/// No native controller is built into the host under this name.
pub const UNKNOWN_NATIVE: &str = "controller.unknown_native";

/// The default controller declares no steps, so it can never be asked to run one.
pub const NO_STEP: &str = "controller.no_step";

/// A manifest `[[events]]` entry names a DocType that none of the app's installed controllers
/// serves, so the step would have nowhere to attach.
pub const EVENT_DOCTYPE_UNSERVED: &str = "controller.event_doctype_unserved";

/// One app names two controllers for the same DocType; an app has one controller per DocType.
pub const DOCTYPE_DUPLICATE: &str = "controller.doctype_duplicate";
