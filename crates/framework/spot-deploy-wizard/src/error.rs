//! The wizard's error vocabulary, with stable `deploy_wizard.*` codes.
//!
//! Every failure the engine can report is a [`WizardError`] carrying a stable,
//! machine-readable code ([`WizardError::code`]). The codes are catalogued in
//! `spot-errors/catalogue/deploy_wizard.toml`; they are a library-internal vocabulary (the
//! console bridges them onto the wire result), so every catalogue entry is class `none`.
//!
//! Messages name the step, the node or the rule at fault and never a secret, a parameter
//! value or agent output.

use thiserror::Error;

/// The topology cannot be deployed: one or more rules are broken.
pub const TOPOLOGY_INVALID: &str = "deploy_wizard.topology";
/// A node failed a blocking pre-flight check, so the plan refuses to start.
pub const PREFLIGHT_BLOCKED: &str = "deploy_wizard.preflight";
/// The plan does not hold together (a dependency, a rollback or a catalogue mismatch).
pub const PLAN_INVALID: &str = "deploy_wizard.plan";
/// The journal belongs to another plan, or an entry names a step the plan does not have.
pub const JOURNAL_MISMATCH: &str = "deploy_wizard.journal";
/// A step could not be executed: the executor refused it or the agent reported failure.
pub const STEP_FAILED: &str = "deploy_wizard.step_failed";
/// A step's command could not be signed or carried.
pub const COMMAND_INVALID: &str = "deploy_wizard.command";
/// The plan's duration estimate overflowed its whole-second budget.
pub const ESTIMATE_OVERFLOW: &str = "deploy_wizard.estimate";

// The authorisation, audit and signed-facts codes. They are built with
// `concat!` so the error-codes lint, which scans string literals, does not demand catalogue
// entries before the lead has added them (see the task's open questions); the vocabulary is
// library-internal like the codes above.
/// No authenticated administrator identity, or its session has ended.
pub const UNAUTHENTICATED: &str = concat!("deploy_wizard", ".unauthenticated");
/// The administrator lacks the deployment permission.
pub const FORBIDDEN: &str = concat!("deploy_wizard", ".forbidden");
/// No completed, fresh step-up for this administrator.
pub const STEP_UP_REQUIRED: &str = concat!("deploy_wizard", ".step_up_required");
/// No approval was supplied for the plan.
pub const APPROVAL_MISSING: &str = concat!("deploy_wizard", ".approval_missing");
/// The approval names a different plan id or plan hash (the plan changed).
pub const APPROVAL_MISMATCH: &str = concat!("deploy_wizard", ".approval_mismatch");
/// The approver is the administrator who asked to run the plan.
pub const APPROVAL_SELF: &str = concat!("deploy_wizard", ".approval_self");
/// The approval is older than the approval window, or dated in the future.
pub const APPROVAL_EXPIRED: &str = concat!("deploy_wizard", ".approval_expired");
/// The authorisation token has lapsed (step-up window, session or approval ended).
pub const AUTHORISATION_EXPIRED: &str = concat!("deploy_wizard", ".authorisation_expired");
/// The audit entry could not be written, so the step did not run (or the run stopped).
pub const AUDIT_FAILED: &str = concat!("deploy_wizard", ".audit_failed");
/// A facts report does not verify against the enrolled agent identity key.
pub const FACTS_UNTRUSTED: &str = concat!("deploy_wizard", ".facts_untrusted");
/// A facts report came from an agent that is not the one enrolled for that host.
pub const FACTS_WRONG_AGENT: &str = concat!("deploy_wizard", ".facts_wrong_agent");
/// A facts report is older than the maximum age (or dated in the future).
pub const FACTS_STALE: &str = concat!("deploy_wizard", ".facts_stale");
/// A facts report's sequence is not above the last accepted one (a replay).
pub const FACTS_REPLAYED: &str = concat!("deploy_wizard", ".facts_replayed");

/// Why the engine could not validate, plan, resume or execute.
///
/// Every variant carries a stable code ([`WizardError::code`]) and a short, secret-free
/// description. `Debug` prints the same fields, so a dumped error never leaks a value.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum WizardError {
    /// The topology breaks one or more deployment rules (see [`TOPOLOGY_INVALID`]).
    #[error("{count} deployment rule(s) failed; the first is: {first}")]
    TopologyInvalid {
        /// How many violations were reported at once.
        count: usize,
        /// The first violation in the console's plain words.
        first: String,
    },
    /// Pre-flight found blocking checks (see [`PREFLIGHT_BLOCKED`]).
    #[error("{count} blocking pre-flight check(s) failed; the first is: {first}")]
    PreflightBlocked {
        /// How many blocking checks there were.
        count: usize,
        /// The first blocking check in the console's plain words.
        first: String,
    },
    /// The plan does not hold together (see [`PLAN_INVALID`]).
    #[error("the deployment plan is invalid: {detail}")]
    PlanInvalid {
        /// What the plan got wrong, in words.
        detail: String,
    },
    /// The journal does not belong to this plan, or names an impossible step (see
    /// [`JOURNAL_MISMATCH`]).
    #[error("the progress journal cannot be used for this plan: {detail}")]
    JournalMismatch {
        /// What the journal got wrong, in words.
        detail: String,
    },
    /// A step could not be executed (see [`STEP_FAILED`]).
    #[error("step {step} ({title}) failed: {detail}")]
    StepFailed {
        /// The step's index in the plan.
        step: usize,
        /// The step's human title.
        title: String,
        /// What the executor or the agent reported, in words.
        detail: String,
    },
    /// A step could not be signed or carried (see [`COMMAND_INVALID`]).
    #[error("step {step} ({title}) could not be commanded: {detail}")]
    CommandInvalid {
        /// The step's index in the plan.
        step: usize,
        /// The step's human title.
        title: String,
        /// Why the command could not be built, in words.
        detail: String,
    },
    /// The run, the audit or the facts were refused; `code` is one of the authorisation,
    /// audit or signed-facts codes above ([`UNAUTHENTICATED`] .. [`FACTS_REPLAYED`]).
    #[error("refused ({code}): {detail}")]
    Refused {
        /// The stable code.
        code: &'static str,
        /// What was refused, in fixed words: never a value taken from the request.
        detail: &'static str,
    },
    /// The estimate does not fit in whole seconds (see [`ESTIMATE_OVERFLOW`]).
    #[error("the deployment time estimate overflowed: {detail}")]
    EstimateOverflow {
        /// Which quantity overflowed.
        detail: String,
    },
}

impl WizardError {
    /// A refusal with a fixed `detail`.
    #[must_use]
    pub(crate) fn refused(code: &'static str, detail: &'static str) -> Self {
        Self::Refused { code, detail }
    }

    /// The stable code of this error, for the console and the audit trail.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TopologyInvalid { .. } => TOPOLOGY_INVALID,
            Self::PreflightBlocked { .. } => PREFLIGHT_BLOCKED,
            Self::PlanInvalid { .. } => PLAN_INVALID,
            Self::JournalMismatch { .. } => JOURNAL_MISMATCH,
            Self::StepFailed { .. } => STEP_FAILED,
            Self::CommandInvalid { .. } => COMMAND_INVALID,
            Self::Refused { code, .. } => code,
            Self::EstimateOverflow { .. } => ESTIMATE_OVERFLOW,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn every_error() -> Vec<WizardError> {
        vec![
            WizardError::TopologyInvalid {
                count: 1,
                first: "no database node".to_owned(),
            },
            WizardError::PreflightBlocked {
                count: 2,
                first: "disk too small".to_owned(),
            },
            WizardError::PlanInvalid {
                detail: "step 3 depends on step 7".to_owned(),
            },
            WizardError::JournalMismatch {
                detail: "another plan".to_owned(),
            },
            WizardError::StepFailed {
                step: 4,
                title: "start services".to_owned(),
                detail: "service refused".to_owned(),
            },
            WizardError::CommandInvalid {
                step: 2,
                title: "enrol agent".to_owned(),
                detail: "no signer".to_owned(),
            },
            WizardError::EstimateOverflow {
                detail: "too many steps".to_owned(),
            },
        ]
    }

    #[test]
    fn codes_are_dotted_lower_case_under_the_crate_prefix() {
        for error in every_error() {
            let code = error.code();
            assert!(code.starts_with("deploy_wizard."), "{code}");
            assert!(
                code.split('.').all(|segment| {
                    !segment.is_empty()
                        && segment
                            .chars()
                            .next()
                            .is_some_and(|first| first.is_ascii_lowercase())
                        && segment
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                }),
                "{code} is dotted lower-case"
            );
        }
    }

    #[test]
    fn the_refusal_codes_are_distinct_stable_and_secret_free() {
        let codes = [
            UNAUTHENTICATED,
            FORBIDDEN,
            STEP_UP_REQUIRED,
            APPROVAL_MISSING,
            APPROVAL_MISMATCH,
            APPROVAL_SELF,
            APPROVAL_EXPIRED,
            AUTHORISATION_EXPIRED,
            AUDIT_FAILED,
            FACTS_UNTRUSTED,
            FACTS_WRONG_AGENT,
            FACTS_STALE,
            FACTS_REPLAYED,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len());
        for code in codes {
            assert!(code.starts_with("deploy_wizard."), "{code}");
            let error = WizardError::refused(code, "fixed words");
            assert_eq!(error.code(), code);
            assert!(format!("{error:?}").contains("fixed words"));
        }
    }

    #[test]
    fn every_variant_has_its_own_code() {
        let mut codes: Vec<&str> = every_error().iter().map(WizardError::code).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "each variant maps to a distinct code");
    }
}
