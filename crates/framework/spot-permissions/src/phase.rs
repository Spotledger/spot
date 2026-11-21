//! ADR-51 phase mapping: which permission each phase of an operation needs.
//!
//! ADR-51 gives every operation four phases (`before_`, `validate_`, `after_`,
//! `<op>_committed`; save-level events cover create and update, and `load` and
//! `field_changed` are read-only). Each phase runs under a *role*, and the role decides whose
//! permissions it carries:
//!
//! - **derive (before)** and **validate** run "as the user": they ask the user's question, so
//!   the engine's permission check is the user's (`Check::AsUser`).
//! - **act (after)** runs in the user's transaction but may write other documents through
//!   their own lifecycle *in it*; the write still demands the **user's** permission
//!   (`Check::AsUser`), the same rule the engine host uses.
//! - **notify (committed)** runs after commit, delivered from the outbox, and its handlers run
//!   **as the system** (`Check::AsSystem`): a committed hook that creates a follow-up document
//!   cannot depend on the original user still holding the right.
//!
//! This module does **not** decide when phases run — `lifecycle::sequence` owns that order and
//! WP-DOC-04's engine executes it. It answers only: "if code at this phase of this operation
//! needs a permission decision, whose rights does `can` evaluate?"

use spot_app_api::lifecycle::Phase;
use spot_events_contract::lifecycle::Operation;

/// Whose permission a phase's check asks about, per ADR-51's phase table
/// (derive/validate/act run as the user; notify runs as the system).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Check {
    /// The check is the acting user's: the decision is about their rights.
    AsUser,
    /// The check is the platform's: committed steps run as the system.
    AsSystem,
}

impl Check {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Check::AsUser => "as_user",
            Check::AsSystem => "as_system",
        }
    }
}

/// The permission checks one operation's phases need, in phase order (before, validate,
/// after, committed): exactly ADR-51's phase table for that operation.
///
/// A phase's entry is `(phase, check)`. The document engine asks `can` once per phase it
/// runs; this function is the mapping from operation to the checks its phases make. The
/// operation argument stays in the signature because callers pass the operation they are
/// running; the table is the same for all seven, because ADR-51 gives every operation the
/// same four phases whose roles (and so whose permission questions) do not differ.
#[must_use]
pub fn phase_requirements(_op: Operation) -> Vec<(Phase, Check)> {
    // Save-level events (before_save, validate_save, …) run alongside the operation's own at
    // every phase (ADR-51), and their permission question is the same one, so one entry per
    // phase is enough. Derive, validate and act run as the user; committed steps run as the
    // system (ADR-51's phase table).
    vec![
        (Phase::Before, Check::AsUser),
        (Phase::Validate, Check::AsUser),
        (Phase::After, Check::AsUser),
        (Phase::Committed, Check::AsSystem),
    ]
}

/// The check one document-reading event needs: `load` and `field_changed` are read-only
/// events outside any transaction (ADR-51), running as the user.
#[must_use]
pub fn read_event_check() -> (Phase, Check) {
    (Phase::Read, Check::AsUser)
}
