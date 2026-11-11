//! How long a deployment will take, so the wizard's progress screen can promise a finish
//! time before anything runs.
//!
//! Each [`StepKind`] has a base duration from a documented table, measured on the
//! reference hardware the blueprint calls for. [`Speed`] scales that table for a faster or
//! slower fleet, in whole basis points — never a float (ADR-12 territory): 75 % for a fast
//! fleet, 100 % for reference, 150 % for a slow one. [`estimate`] walks the plan's
//! dependencies and returns the **critical path**, so a plan that runs independent steps in
//! parallel estimates lower than the sum of its steps.
//!
//! [`progress`] repeats that walk with the journal's completed steps costing nothing, which
//! is what the progress screen shows after a pause or a failure.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::WizardError;
use crate::journal::{Journal, completed_steps};
use crate::plan::{DeployPlan, StepKind};

/// The reference duration of one step, in whole seconds.
///
/// The table is the wizard's budget: agent enrolment is a token exchange; hardening is the
/// longest software step because it applies the baseline control by control; installing the
/// runtime unpacks a signed bundle; database creation dominates because it initialises the
/// cluster; a replica clones the primary over the management network; roles, profile, start
/// and join are settings writes and service starts; verification reads health endpoints.
#[must_use]
pub fn base_secs(kind: StepKind) -> u64 {
    match kind {
        StepKind::EnrolAgent => 45,
        StepKind::HardenBaseline => 180,
        StepKind::InstallRuntime => 150,
        StepKind::ApplyProfile => 25,
        StepKind::ConfigureRoles => 20,
        StepKind::InitialiseDatabase => 240,
        StepKind::CreateReplica => 300,
        StepKind::StartServices => 45,
        StepKind::JoinCluster => 30,
        StepKind::VerifyHealth => 20,
    }
}

/// How fast the fleet is against the reference table.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    /// A fast fleet: 75 % of the reference durations.
    Fast,
    /// The reference fleet: the table as written.
    #[default]
    Reference,
    /// A slow fleet: 150 % of the reference durations.
    Slow,
}

impl Speed {
    /// The scale in basis points of the reference table.
    #[must_use]
    pub fn basis_points(self) -> u64 {
        match self {
            Self::Fast => 75,
            Self::Reference => 100,
            Self::Slow => 150,
        }
    }
}

/// The wizard's time budget for a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    /// The wall-clock seconds the plan needs when independent steps run in parallel: the
    /// longest dependency chain.
    pub total_secs: u64,
    /// The seconds every step would take one after another, with no parallelism.
    pub serial_secs: u64,
    /// Each step's own duration, in plan order.
    pub per_step_secs: Vec<u64>,
    /// The steps on the critical path, in order. Running any of them slower moves the
    /// finish time; the others have slack.
    pub critical_path: Vec<usize>,
}

impl Estimate {
    /// Whether the plan uses parallelism: the critical path is shorter than the serial sum.
    #[must_use]
    pub fn parallel(&self) -> bool {
        self.total_secs < self.serial_secs
    }
}

/// The plan's estimate, with `done` steps costing nothing.
///
/// # Errors
/// [`WizardError::EstimateOverflow`] when a duration does not fit in whole seconds.
pub fn estimate_after(
    plan: &DeployPlan,
    speed: Speed,
    done: &BTreeSet<usize>,
) -> Result<Estimate, WizardError> {
    if plan.is_empty() {
        return Err(WizardError::PlanInvalid {
            detail: "the plan has no steps to estimate".to_owned(),
        });
    }
    let mut per_step_secs = Vec::with_capacity(plan.len());
    for step in plan.steps() {
        let base = if done.contains(&step.index) {
            0
        } else {
            base_secs(step.kind)
        };
        let scaled = base
            .checked_mul(speed.basis_points())
            .and_then(|value| value.checked_add(99))
            .map(|value| value / 100)
            .ok_or_else(|| WizardError::EstimateOverflow {
                detail: format!("step {} at the chosen speed", step.index),
            })?;
        per_step_secs.push(scaled);
    }

    let mut finish = Vec::with_capacity(plan.len());
    for step in plan.steps() {
        let mut start: u64 = 0;
        for dependency in &step.depends_on {
            start = start.max(finish.get(*dependency).copied().unwrap_or(0));
        }
        finish.push(
            start
                .checked_add(per_step_secs[step.index])
                .ok_or_else(|| WizardError::EstimateOverflow {
                    detail: format!("the finish time of step {}", step.index),
                })?,
        );
    }

    let total_secs = finish.iter().copied().max().unwrap_or(0);
    let serial_secs = per_step_secs.iter().try_fold(0_u64, |sum, secs| {
        sum.checked_add(*secs)
            .ok_or_else(|| WizardError::EstimateOverflow {
                detail: "the serial sum of every step".to_owned(),
            })
    })?;

    let mut critical_path = Vec::new();
    if let Some(last) = finish
        .iter()
        .enumerate()
        .max_by_key(|(index, secs)| (**secs, std::cmp::Reverse(*index)))
        .map(|(index, _)| index)
    {
        let mut cursor = Some(last);
        while let Some(index) = cursor {
            critical_path.push(index);
            let step = plan.step(index).ok_or_else(|| WizardError::PlanInvalid {
                detail: format!("step {index} is missing from the plan"),
            })?;
            cursor = step
                .depends_on
                .iter()
                .copied()
                .max_by_key(|dependency| finish.get(*dependency).copied().unwrap_or(0));
        }
    }
    critical_path.reverse();

    Ok(Estimate {
        total_secs,
        serial_secs,
        per_step_secs,
        critical_path,
    })
}

/// The plan's estimate from the start.
///
/// # Errors
/// As [`estimate_after`].
pub fn estimate(plan: &DeployPlan, speed: Speed) -> Result<Estimate, WizardError> {
    estimate_after(plan, speed, &BTreeSet::new())
}

/// What the progress screen shows: how far the deployment got and how much is left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// How many steps the plan has.
    pub total_steps: usize,
    /// How many have succeeded.
    pub done_steps: usize,
    /// Completion in basis points, 0 to 10 000.
    pub percent_bp: u32,
    /// Whole seconds left, assuming independent steps still run in parallel.
    pub remaining_secs: u64,
    /// The next step to run, when the plan is not finished.
    pub current: Option<usize>,
    /// The whole plan's full estimate, for the "about N minutes" promise.
    pub total_secs: u64,
}

/// The progress of `plan` according to `journal`, at `speed`.
///
/// # Errors
/// [`WizardError::JournalMismatch`] when the journal belongs to another plan, and
/// [`WizardError::EstimateOverflow`] when the remainder does not fit.
pub fn progress(
    plan: &DeployPlan,
    journal: &dyn Journal,
    speed: Speed,
) -> Result<Progress, WizardError> {
    let done = completed_steps(plan, journal)?;
    let total_steps = plan.len();
    let done_steps = done.len();
    let percent_bp = if total_steps == 0 {
        0
    } else {
        u32::try_from(done_steps as u64 * 10_000 / total_steps as u64).unwrap_or(10_000)
    };
    let remaining = estimate_after(plan, speed, &done)?;
    let whole = estimate(plan, speed)?;
    let current = plan
        .ready(&done)
        .into_iter()
        .find(|index| !done.contains(index));
    Ok(Progress {
        total_steps,
        done_steps,
        percent_bp,
        remaining_secs: remaining.total_secs,
        current,
        total_secs: whole.total_secs,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::journal::MemoryJournal;
    use crate::topology::{NodeSpec, Profile, Role, Topology};

    fn node(host: &str, roles: &[Role]) -> NodeSpec {
        NodeSpec::new(host, roles).unwrap()
    }

    fn reference() -> Topology {
        Topology::new(
            vec![
                node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                node("db1.example.com", &[Role::Database]),
                node("db2.example.com", &[Role::Database]),
            ],
            Profile::ClusterBasic,
        )
        .with_web_address("erp.example.com")
    }

    #[test]
    fn the_reference_three_node_plan_fits_the_thirty_minute_budget() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let estimate = estimate(&plan, Speed::Reference).unwrap();
        assert!(
            estimate.total_secs < 30 * 60,
            "the reference plan estimates {} seconds",
            estimate.total_secs
        );
        assert!(
            estimate.parallel(),
            "independent nodes run in parallel: {} seconds against {} serial",
            estimate.total_secs,
            estimate.serial_secs
        );
        assert!(estimate.serial_secs > estimate.total_secs);
        assert!(!estimate.critical_path.is_empty());
    }

    #[test]
    fn speed_scales_the_estimate_in_whole_seconds() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let fast = estimate(&plan, Speed::Fast).unwrap();
        let reference = estimate(&plan, Speed::Reference).unwrap();
        let slow = estimate(&plan, Speed::Slow).unwrap();
        assert!(fast.total_secs < reference.total_secs);
        assert!(reference.total_secs < slow.total_secs);
        for secs in &reference.per_step_secs {
            assert!(*secs <= 300);
        }
        assert_eq!(base_secs(StepKind::InitialiseDatabase) * 150 / 100, 360);
    }

    #[test]
    fn every_step_kind_has_a_positive_duration() {
        for kind in [
            StepKind::EnrolAgent,
            StepKind::HardenBaseline,
            StepKind::InstallRuntime,
            StepKind::ApplyProfile,
            StepKind::ConfigureRoles,
            StepKind::InitialiseDatabase,
            StepKind::CreateReplica,
            StepKind::StartServices,
            StepKind::JoinCluster,
            StepKind::VerifyHealth,
        ] {
            assert!(base_secs(kind) > 0, "{} has a duration", kind.name());
        }
    }

    #[test]
    fn the_critical_path_names_the_database_chain() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let estimate = estimate(&plan, Speed::Reference).unwrap();
        let kinds: Vec<StepKind> = estimate
            .critical_path
            .iter()
            .map(|index| plan.step(*index).unwrap().kind)
            .collect();
        assert!(kinds.contains(&StepKind::InitialiseDatabase));
        assert!(kinds.contains(&StepKind::CreateReplica));
        assert_eq!(*kinds.last().unwrap(), StepKind::VerifyHealth);
    }

    #[test]
    fn progress_counts_done_steps_and_projects_the_remainder() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let fresh = progress(&plan, &journal, Speed::Reference).unwrap();
        assert_eq!(fresh.done_steps, 0);
        assert_eq!(fresh.percent_bp, 0);
        assert!(fresh.current.is_some());
        assert_eq!(
            fresh.total_secs,
            estimate(&plan, Speed::Reference).unwrap().total_secs
        );

        let whole = estimate(&plan, Speed::Reference).unwrap();
        let first = *whole.critical_path.first().unwrap();
        journal.started(first, 1_000).unwrap();
        journal.succeeded(first, 1_045).unwrap();
        let after = progress(&plan, &journal, Speed::Reference).unwrap();
        assert_eq!(after.done_steps, 1);
        assert!(after.percent_bp > 0);
        assert!(after.remaining_secs < fresh.remaining_secs);
    }
}
