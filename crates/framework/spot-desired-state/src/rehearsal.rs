//! Rehearsal: a structural dry run of a bulk plan before anything is applied.
//!
//! A rehearsal does not touch a server. It proves the plan is complete enough to attempt:
//! every step names a server and at least one typed operation, step ids are unique, and the
//! plan has a group. It records the same digest as the approval, so a run can require both
//! proofs to match the plan it holds.
//!
//! The apply phase itself is [`crate::BulkRun`]; a failed verify there is a separate outcome
//! from a failed rehearsal.

use serde::{Deserialize, Serialize};

use crate::ids::PlanDigest;
use crate::plan::BulkPlan;

/// One structural problem the rehearsal found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehearsalProblem {
    /// The step id the problem belongs to, or `-` for the plan as a whole.
    pub step: String,
    /// What is wrong, in a sentence.
    pub message: String,
}

/// The result of rehearsing a plan.
///
/// A report can only come from [`rehearse`] in this crate: its fields are private, it has no
/// public constructor and it cannot be deserialized, so neither a caller nor the server the
/// plan describes can hand over a passing report it did not earn. Whether it passed is
/// derived from the problems it holds and the steps it checked, never stored as a flag.
///
/// A hand-built report does not compile:
///
/// ```compile_fail
/// use spot_desired_state::{PlanDigest, RehearsalReport};
///
/// let forged = RehearsalReport {
///     digest: PlanDigest::from_hex("0".repeat(64)),
///     checked_steps: 1,
///     servers: 1,
///     operations: 1,
///     problems: Vec::new(),
/// };
/// ```
///
/// Nor does a report read from text:
///
/// ```compile_fail
/// let forged: spot_desired_state::RehearsalReport = serde_json::from_str("{}").unwrap();
/// ```
///
/// The report binds to the plan by its digest only. This crate has no signing helper and may not
/// add a dependency, so a report that crossed a process boundary would not be authenticated; it
/// therefore has no `Deserialize`, and the runner must rehearse in the same process that runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RehearsalReport {
    digest: PlanDigest,
    checked_steps: usize,
    servers: usize,
    operations: usize,
    problems: Vec<RehearsalProblem>,
}

impl RehearsalReport {
    /// Whether the plan may be attempted: it was rehearsed with at least one step and the
    /// rehearsal found no problem.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.problems.is_empty() && self.checked_steps > 0
    }

    /// The plan digest the rehearsal checked.
    #[must_use]
    pub fn digest(&self) -> &PlanDigest {
        &self.digest
    }

    /// Whether this report was made for exactly `plan` as it is now.
    #[must_use]
    pub fn covers(&self, plan: &BulkPlan) -> bool {
        self.digest == plan.digest()
    }

    /// How many steps were checked.
    #[must_use]
    pub fn checked_steps(&self) -> usize {
        self.checked_steps
    }

    /// How many servers the plan covers.
    #[must_use]
    pub fn servers(&self) -> usize {
        self.servers
    }

    /// How many typed operations the plan holds.
    #[must_use]
    pub fn operations(&self) -> usize {
        self.operations
    }

    /// Every problem found, in step order then plan order.
    #[must_use]
    pub fn problems(&self) -> &[RehearsalProblem] {
        &self.problems
    }
}

/// Rehearses `plan` structurally. The only way to obtain a [`RehearsalReport`].
#[must_use]
pub fn rehearse(plan: &BulkPlan) -> RehearsalReport {
    let mut problems = Vec::new();
    let step_label = |index: usize| {
        plan.steps
            .get(index)
            .map_or_else(|| "-".to_owned(), |step| step.id.clone())
    };

    if plan.steps.is_empty() {
        problems.push(RehearsalProblem {
            step: "-".to_owned(),
            message: "the plan has no step".to_owned(),
        });
    }
    if plan.group.trim().is_empty() && !plan.steps.is_empty() {
        problems.push(RehearsalProblem {
            step: "-".to_owned(),
            message: "the plan names no group".to_owned(),
        });
    }

    let mut ids = std::collections::BTreeSet::new();
    for (index, step) in plan.steps.iter().enumerate() {
        if step.id.trim().is_empty() {
            problems.push(RehearsalProblem {
                step: step_label(index),
                message: "the step has no id".to_owned(),
            });
        } else if !ids.insert(step.id.clone()) {
            problems.push(RehearsalProblem {
                step: step.id.clone(),
                message: "the step id is used twice".to_owned(),
            });
        }
        if step.group.trim().is_empty() {
            problems.push(RehearsalProblem {
                step: step.id.clone(),
                message: "the step names no group".to_owned(),
            });
        }
        if step.operations.is_empty() {
            problems.push(RehearsalProblem {
                step: step.id.clone(),
                message: "the step has no operation".to_owned(),
            });
        }
    }

    RehearsalReport {
        digest: plan.digest(),
        checked_steps: plan.steps.len(),
        servers: plan.servers().len(),
        operations: plan.operation_count(),
        problems,
    }
}
