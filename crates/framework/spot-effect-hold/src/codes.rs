//! The stable `effect_hold.*` error codes of the side-effect hold.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/effect_hold.toml` with its class, a
//! one-sentence summary and a remedy. The codes carry the `effect_hold.` prefix because the
//! catalogue requires a file's name to be its entries' prefix (`catalogue/README.md`); the task
//! brief's shorthand `hold.*` cannot coexist with the file `effect_hold.toml`.

/// A site already has a live hold; a second `begin` is refused.
pub const ALREADY_HELD: &str = "effect_hold.already_held";
/// A transition was asked for on a site that is not held.
pub const NOT_HELD: &str = "effect_hold.not_held";
/// A release, discard or confirmation named a different migration than the live hold.
pub const WRONG_MIGRATION: &str = "effect_hold.wrong_migration";
/// The hold ledger could not be read, so the site is treated as held (fail closed).
pub const STATE_UNKNOWN: &str = "effect_hold.state_unknown";
/// A configuration tried to make fiscal transmissions holdable.
pub const FISCAL_CANNOT_BE_HELD: &str = "effect_hold.fiscal_cannot_be_held";
/// A held effect is older than its class' `max_hold` and needs explicit confirmation.
pub const RELEASE_NEEDS_CONFIRMATION: &str = "effect_hold.release_needs_confirmation";
/// The hold ledger is unavailable; the effect is held fail-closed but not yet recorded.
pub const LEDGER_UNAVAILABLE: &str = "effect_hold.ledger_unavailable";
/// A dispatcher failed while releasing a held effect.
pub const DISPATCH_FAILED: &str = "effect_hold.dispatch_failed";
/// A class asked to be `never_hold` without a lead-approved allow-list entry.
pub const NEVER_HOLD_NOT_ALLOWED: &str = "effect_hold.never_hold_not_allowed";
/// An identifier or argument is not in its documented form.
pub const INVALID_INPUT: &str = "effect_hold.invalid_input";
/// Another node is already releasing this site's hold.
pub const RELEASE_IN_PROGRESS: &str = "effect_hold.release_in_progress";
/// Recording a hold transition in the audit chain failed.
pub const AUDIT_FAILED: &str = "effect_hold.audit_failed";
