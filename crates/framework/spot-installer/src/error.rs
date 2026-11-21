//! The `installer.*` refusals: every reason a plan is not produced, each with its own stable
//! code.
//!
//! A [`PlanError`] is a code and a plain-language message. Callers match on the code, never on
//! the text; `installer.toml` in `spot-errors` carries the catalogue entry for each one, with a
//! remedy a person can act on.

/// `installer.unverified`: a candidate bundle the caller marked unverified is needed.
pub const UNVERIFIED: &str = "installer.unverified";
/// `installer.requires_host`: the app's `requires_host` range excludes this host's version.
pub const REQUIRES_HOST: &str = "installer.requires_host";
/// `installer.wit_world_mismatch`: the bundle targets a WIT world this host does not serve.
pub const WIT_WORLD_MISMATCH: &str = "installer.wit_world_mismatch";
/// `installer.downgrade`: a change would move an app to a lower version without a downgrade request.
pub const DOWNGRADE: &str = "installer.downgrade";
/// `installer.downgrade_unsafe`: an explicit downgrade whose target would run a data or schema patch.
pub const DOWNGRADE_UNSAFE: &str = "installer.downgrade_unsafe";
/// `installer.digest_changed`: the same version is offered with a different bundle digest.
pub const DIGEST_CHANGED: &str = "installer.digest_changed";
/// `installer.already_installed`: an install names an app the site already has.
pub const ALREADY_INSTALLED: &str = "installer.already_installed";
/// `installer.not_installed`: an upgrade, downgrade or uninstall names an app the site does not have.
pub const NOT_INSTALLED: &str = "installer.not_installed";
/// `installer.has_dependents`: an uninstall would remove an app another installed app depends on.
pub const HAS_DEPENDENTS: &str = "installer.has_dependents";
/// `installer.patch_removed`: the new manifest drops a patch id the site has already applied.
pub const PATCH_REMOVED: &str = "installer.patch_removed";
/// `installer.missing_dependency`: a required app has no bundle in the catalogue at all.
pub const MISSING_DEPENDENCY: &str = "installer.missing_dependency";
/// `installer.conflicting_requirement`: catalogue versions exist but none satisfies every requirement.
pub const CONFLICTING_REQUIREMENT: &str = "installer.conflicting_requirement";
/// `installer.breaks_dependent`: a chosen version violates an installed app's requirement.
pub const BREAKS_DEPENDENT: &str = "installer.breaks_dependent";
/// `installer.dependency_cycle`: the resolved dependencies form a cycle.
pub const DEPENDENCY_CYCLE: &str = "installer.dependency_cycle";
/// `installer.no_candidate`: the catalogue holds no bundle matching a request's version range.
pub const NO_CANDIDATE: &str = "installer.no_candidate";
/// `installer.plan_not_approved`: the plan's hash is not the hash an administrator approved.
pub const PLAN_NOT_APPROVED: &str = "installer.plan_not_approved";
/// `installer.needs_approval`: the plan asks for capabilities an administrator has not approved.
pub const NEEDS_APPROVAL: &str = "installer.needs_approval";
/// `installer.lease_held`: another installer holds this site's lease, so the run did not start.
pub const LEASE_HELD: &str = "installer.lease_held";
/// `installer.lease_lost`: the per-site lease was lost mid-run; the journal is left for the next
/// holder to resume.
pub const LEASE_LOST: &str = "installer.lease_lost";
/// `installer.rollback_failed`: a step failed and undoing an earlier step failed too; the site
/// needs a person, and stays flagged `installing`.
pub const ROLLBACK_FAILED: &str = "installer.rollback_failed";
/// `installer.journal_corrupt`: the resume journal is missing rows or mixes two runs, so it
/// cannot be trusted.
pub const JOURNAL_CORRUPT: &str = "installer.journal_corrupt";
/// `installer.step_failed`: a trait method failed while running a plan step. The message names
/// the step; the code a real adapter raises is the adapter's own.
pub const STEP_FAILED: &str = "installer.step_failed";
/// `installer.interrupted`: `resume` found a run that stopped before its commit and put the site
/// back.
pub const INTERRUPTED: &str = "installer.interrupted";
/// `installer.no_plan`: `run_many` was given a site with no approved plan.
pub const NO_PLAN: &str = "installer.no_plan";

/// A refusal to build a plan: a stable `installer.*` code and a message for the administrator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct PlanError {
    /// The stable machine-readable code, for the API error envelope.
    pub code: &'static str,
    /// What went wrong, phrased for the administrator or the app author.
    pub message: String,
}

impl PlanError {
    /// Builds a refusal from its code and message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
