//! Down-patches: the declared `down` step of every applied reversible patch, in reverse order.
//!
//! A down-patch is the rollback window of ADR-22 where the previous binary runs against the
//! expanded schema. The declaration is WP-MIG-02's [`PatchDecl`]; this crate never invents a
//! down-step. An applied patch with `reversible = false`, or with no declared `down`, stops the
//! whole plan before any step runs, with [`Error::Irreversible`] naming every such patch, so an
//! operator sees the complete blocker set at once instead of discovering it halfway through.
//!
//! Execution uses exactly the apply engine's [`StepRunner`](spot_migration_apply::StepRunner),
//! with the plan digest, site and time in the [`StepContext`](spot_migration_apply::StepContext).
//! The maintenance gate is closed before the first step and opened only after the optional
//! invariant pass; a failed step leaves it closed.

use std::sync::Arc;

use spot_core::SiteId;
use spot_migration_apply::{AdmissionGate, StepContext, StepRunner};
use spot_migration_plan::{ApplyStep, ApplyStepKind};
use spot_release_meta::PatchDecl;

use crate::error::{Error, Result};
use crate::faults::{Faults, KillPoint, NoFaults};
use crate::invariants::{InvariantPhase, InvariantReport, Invariants};

/// One down-patch step: the applied patch it undoes and the declared step that undoes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownStep {
    /// The applied patch being undone.
    pub patch_id: String,
    /// The declared down-patch id.
    pub down_id: String,
    /// The apply-engine step the runner executes.
    pub step: ApplyStep,
}

/// The complete down-patch plan for the applied patches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownPlan {
    plan_digest: String,
    steps: Vec<DownStep>,
}

impl DownPlan {
    /// Builds the plan from the applied patches in apply order.
    ///
    /// The steps come out in reverse apply order; a single irreversible or down-less patch
    /// refuses the whole plan.
    ///
    /// # Errors
    /// [`Error::Irreversible`] naming every patch that cannot be undone, and
    /// [`Error::InvalidInput`] when the plan digest is empty.
    pub fn build(plan_digest: impl Into<String>, applied: Vec<PatchDecl>) -> Result<Self> {
        let plan_digest = plan_digest.into();
        if plan_digest.trim().is_empty() {
            return Err(Error::InvalidInput {
                what: "plan digest".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        let mut irreversible = Vec::new();
        for patch in &applied {
            if !patch.reversible || patch.down.is_none() {
                irreversible.push(patch.id.clone());
            }
        }
        if !irreversible.is_empty() {
            irreversible.sort();
            irreversible.dedup();
            return Err(Error::Irreversible {
                patches: irreversible,
            });
        }
        let mut steps = Vec::with_capacity(applied.len());
        for patch in applied.into_iter().rev() {
            let Some(down) = patch.down else {
                return Err(Error::Irreversible {
                    patches: vec![patch.id],
                });
            };
            steps.push(DownStep {
                step: ApplyStep {
                    id: down.id.clone(),
                    kind: ApplyStepKind::Patch,
                    change_id: Some(down.id.clone()),
                    idempotency_key: format!("{plan_digest}:down:{}", down.id),
                },
                patch_id: patch.id,
                down_id: down.id,
            });
        }
        Ok(Self { plan_digest, steps })
    }

    /// The plan digest.
    #[must_use]
    pub fn plan_digest(&self) -> &str {
        &self.plan_digest
    }

    /// The down-patch steps in execution order.
    #[must_use]
    pub fn steps(&self) -> &[DownStep] {
        &self.steps
    }
}

/// Everything one down-patch run needs.
#[derive(Clone, Debug)]
pub struct DownRequest {
    /// The digest of the plan whose patches are being undone.
    pub plan_digest: String,
    /// The applied patches, in apply order.
    pub applied: Vec<PatchDecl>,
    /// Unix microseconds; the runner reads no clock.
    pub now: i64,
}

impl DownRequest {
    /// A request for `plan_digest`.
    #[must_use]
    pub fn new(plan_digest: impl Into<String>, applied: Vec<PatchDecl>, now: i64) -> Self {
        Self {
            plan_digest: plan_digest.into(),
            applied,
            now,
        }
    }
}

/// What one down-patch step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownStepReport {
    /// The step's index in the down plan.
    pub index: usize,
    /// The applied patch being undone.
    pub patch_id: String,
    /// The down-patch id.
    pub down_id: String,
    /// The checkpoint the runner recorded, if any.
    pub checkpoint: Option<String>,
    /// A secret-free detail.
    pub detail: String,
    /// The failure, when the step failed.
    pub error: Option<String>,
}

/// The result of one down-patch run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownReport {
    /// The plan digest.
    pub plan_digest: String,
    /// One report per step that was attempted, in order.
    pub steps: Vec<DownStepReport>,
    /// The invariant pass, when one was registered.
    pub invariants: Option<InvariantReport>,
    /// Why the run halted, when it did.
    pub halted: Option<String>,
    /// Whether the gate was opened (only a completed run opens it).
    pub gate_open: bool,
}

impl DownReport {
    /// Whether every step ran and every registered invariant held.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.halted.is_none() && self.gate_open
    }

    /// The steps that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&DownStepReport> {
        self.steps
            .iter()
            .filter(|step| step.error.is_some())
            .collect()
    }
}

/// What a down-patch run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownOutcome {
    /// Every down-patch ran and the gate opened.
    Completed(DownReport),
    /// A step or an invariant failed; the gate stayed closed.
    Halted(DownReport),
}

impl DownOutcome {
    /// The report, whichever way the run ended.
    #[must_use]
    pub fn report(&self) -> &DownReport {
        match self {
            Self::Completed(report) | Self::Halted(report) => report,
        }
    }
}

/// Runs a down-patch plan through the apply engine's step runner.
pub struct DownPatcher {
    site: SiteId,
    runner: Arc<dyn StepRunner>,
    gate: Arc<dyn AdmissionGate>,
    invariants: Option<Arc<dyn Invariants>>,
    faults: Arc<dyn Faults>,
}

impl DownPatcher {
    /// A patcher for `site` over `runner`, closing `gate` while it runs.
    #[must_use]
    pub fn new(site: SiteId, runner: Arc<dyn StepRunner>, gate: Arc<dyn AdmissionGate>) -> Self {
        Self {
            site,
            runner,
            gate,
            invariants: None,
            faults: Arc::new(NoFaults),
        }
    }

    /// The same patcher checking `invariants` before it opens the gate.
    #[must_use]
    pub fn with_invariants(mut self, invariants: Arc<dyn Invariants>) -> Self {
        self.invariants = Some(invariants);
        self
    }

    /// The same patcher with a fault-injection switch.
    #[must_use]
    pub fn with_faults(mut self, faults: Arc<dyn Faults>) -> Self {
        self.faults = faults;
        self
    }

    /// Runs `request`.
    ///
    /// # Errors
    /// [`Error::Irreversible`] when any patch has no down-step, and a gate error when the site
    /// cannot be closed or opened. A failed step is a [`DownOutcome::Halted`] report, not an
    /// error, because the site is left closed and nothing was discarded.
    pub async fn run(&self, request: DownRequest) -> Result<DownOutcome> {
        let plan = DownPlan::build(request.plan_digest, request.applied)?;
        self.gate
            .close("down-patches are running", request.now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        let context = StepContext::new(
            &self.site,
            plan.plan_digest(),
            format!("rescue-down:{}", plan.plan_digest()),
            request.now,
        );
        let mut steps = Vec::with_capacity(plan.steps().len());
        for (index, down) in plan.steps().iter().enumerate() {
            self.faults.at(&KillPoint::BeforeDownStep(index))?;
            let descriptor = self.runner.describe(index, &down.step);
            match self.runner.run(&descriptor, &context).await {
                Ok(outcome) => steps.push(DownStepReport {
                    index,
                    patch_id: down.patch_id.clone(),
                    down_id: down.down_id.clone(),
                    checkpoint: outcome.checkpoint.map(|checkpoint| checkpoint.key),
                    detail: outcome.detail,
                    error: None,
                }),
                Err(error) => {
                    steps.push(DownStepReport {
                        index,
                        patch_id: down.patch_id.clone(),
                        down_id: down.down_id.clone(),
                        checkpoint: None,
                        detail: "the down-patch step did not complete".to_owned(),
                        error: Some(error.to_string()),
                    });
                    let report = DownReport {
                        plan_digest: plan.plan_digest().to_owned(),
                        steps,
                        invariants: None,
                        halted: Some(format!(
                            "{}: down-patch {} ({}) failed",
                            crate::codes::DOWN_STEP_FAILED,
                            index,
                            down.down_id
                        )),
                        gate_open: false,
                    };
                    return Ok(DownOutcome::Halted(report));
                }
            }
        }
        let invariants = match &self.invariants {
            Some(invariants) => {
                let report = invariants
                    .check(InvariantPhase::AfterDownPatches)
                    .await
                    .map_err(|error| Error::InvariantsFailed {
                        violations: 0,
                        details: vec![error.to_string()],
                    })?;
                if !report.passed() {
                    let report = DownReport {
                        plan_digest: plan.plan_digest().to_owned(),
                        steps,
                        invariants: Some(report),
                        halted: Some(crate::codes::INVARIANTS_FAILED.to_owned()),
                        gate_open: false,
                    };
                    return Ok(DownOutcome::Halted(report));
                }
                Some(report)
            }
            None => None,
        };
        self.faults.at(&KillPoint::AfterDownPatches)?;
        self.gate
            .open(request.now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        Ok(DownOutcome::Completed(DownReport {
            plan_digest: plan.plan_digest().to_owned(),
            steps,
            invariants,
            halted: None,
            gate_open: true,
        }))
    }
}
