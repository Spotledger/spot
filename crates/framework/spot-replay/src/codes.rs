//! The stable `replay.*` error codes.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/replay.toml` with its class, a
//! one-sentence summary and a remedy. The codes carry the `replay.` prefix because the catalogue
//! requires a file's name to be its entries' prefix.
//!
//! A divergence a replay finds, and a divergence a declared behaviour entry explains, are part of
//! the [`ReplayReport`](crate::ReplayReport), not errors: only a run that cannot continue raises.

/// An identifier, a policy, a budget or a request is not in its documented form.
pub const INVALID_INPUT: &str = "replay.invalid_input";
/// Reading the event journal (the sample's source) failed.
pub const JOURNAL_FAILED: &str = "replay.journal_failed";
/// The document engine failed to re-execute a sampled operation on the clone.
pub const EXECUTION_FAILED: &str = "replay.execution_failed";
/// The site is not held, so replay refuses to run: an unheld replay could let an effect escape.
pub const NOT_HELD: &str = "replay.not_held";
/// An external effect of a never-held class was admitted to proceed during replay; it is refused.
pub const EFFECT_ESCAPED: &str = "replay.effect_escaped";
/// The target a replay was asked to run against is not the clone.
pub const NOT_CLONE: &str = "replay.not_clone";
