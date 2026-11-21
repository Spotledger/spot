//! Explanations: why each step in a plan runs, and which overrides touched it.

use spot_app_api::{EffectDecl, OverrideKind, StepName};

/// An override that affected a step, as the admin listing shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedOverride {
    /// How it changes its target.
    pub kind: OverrideKind,
    /// The step it changes.
    pub target: StepName,
    /// The step that runs instead of or around the target; `None` for `Disable`.
    pub by: Option<StepName>,
    /// The app that declared the override.
    pub declared_by: String,
    /// Why, as the declaring app wrote it.
    pub reason: String,
}

/// Why a planned step is where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// The step runs because a controller declared it at this hook point.
    Declared,
    /// The step runs immediately before `target`, because of an override.
    Before {
        /// The step it precedes.
        target: StepName,
    },
    /// The step runs immediately after `target`, because of an override.
    After {
        /// The step it follows.
        target: StepName,
    },
    /// The step runs in `target`'s place and may run the target once.
    Wrapper {
        /// The wrapped step.
        target: StepName,
    },
    /// The step runs in `target`'s place; the target never runs.
    Replacement {
        /// The replaced step.
        target: StepName,
    },
}

/// One line of an explanation: one planned step, or one declared step that an override disabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainLine {
    /// The step.
    pub step: StepName,
    /// The app that declared it.
    pub app: String,
    /// Its own priority.
    pub priority: i32,
    /// Why it is in the plan.
    pub role: Role,
    /// Every override that affected this line, in (kind, by) order. For a wrapper or replacement
    /// these are all the overrides of the target.
    pub overrides: Vec<AppliedOverride>,
    /// `false` only for a declared step an override disabled: it is not in the plan, and the
    /// line says who removed it.
    pub runs: bool,
    /// The effects this line declares (ADR-080 6.3), in declaration order: those of the step
    /// that runs on the line, then, for a wrapper, those of the step it wraps. For a line with
    /// `runs == false` these are the effects the disabled step would have had, which no longer
    /// happen.
    pub effects: Vec<EffectDecl>,
}
