//! The stable `promotion.*` error codes of the promotion pipeline.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/promotion.toml` with its class, a
//! one-sentence summary and a remedy. The codes carry the `promotion.` prefix because the
//! catalogue requires a file's name to be its entries' prefix.

/// The promotion id is empty or holds a character the audit store refuses.
pub const BAD_ID: &str = "promotion.bad_id";
/// No promotion with this id is stored.
pub const NOT_FOUND: &str = "promotion.not_found";
/// A promotion with this id already exists; ids are never reused.
pub const DUPLICATE: &str = "promotion.duplicate";
/// The promotion is not in the state this step starts from.
pub const BAD_STATE: &str = "promotion.bad_state";
/// The bundle in hand is not the bundle this promotion was requested for.
pub const BUNDLE_MISMATCH: &str = "promotion.bundle_mismatch";
/// The bundle could not be planned against the target.
pub const PLAN_FAILED: &str = "promotion.plan_failed";
/// A blocking conflict or refused change stops the promotion before anything is written.
pub const BLOCKED: &str = "promotion.blocked";
/// The bundle would change nothing on the target.
pub const NOTHING_TO_PROMOTE: &str = "promotion.nothing_to_promote";
/// The plan in hand is not the plan that was approved.
pub const PLAN_CHANGED: &str = "promotion.plan_changed";
/// The target environment's policy requires approvals that are missing, rejected or stale.
pub const APPROVAL_REQUIRED: &str = "promotion.approval_required";
/// The target environment's policy requires a rehearsal before apply.
pub const REHEARSAL_REQUIRED: &str = "promotion.rehearsal_required";
/// The rehearsal reported a failure; apply is refused.
pub const REHEARSAL_FAILED: &str = "promotion.rehearsal_failed";
/// Writing the bundle to the target failed.
pub const APPLY_FAILED: &str = "promotion.apply_failed";
/// Verification found the target does not match the approved plan.
pub const VERIFY_FAILED: &str = "promotion.verify_failed";
/// Restoring the target to its pre-apply state failed.
pub const RESTORE_FAILED: &str = "promotion.restore_failed";
/// A promotion transition could not be recorded in the migration audit store.
pub const AUDIT_FAILED: &str = "promotion.audit_failed";
/// The promotion store could not be read or written.
pub const STORE_FAILED: &str = "promotion.store_failed";
/// The environment approval or rehearsal policy is not usable.
pub const POLICY_INVALID: &str = "promotion.policy_invalid";
/// The promotion would not advance the environment ladder.
pub const BAD_ROUTE: &str = "promotion.bad_route";
/// The target site's environment does not match the plan's target environment.
pub const ENVIRONMENT_MISMATCH: &str = "promotion.environment_mismatch";
/// The approval service refused the approval or rejection (unknown author, wrong role, a
/// claimed role the identity does not hold, or an approver who authored the plan).
pub const APPROVAL_REFUSED: &str = "promotion.approval_refused";
