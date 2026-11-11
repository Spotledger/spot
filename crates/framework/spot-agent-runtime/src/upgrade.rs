//! Upgrades through the migration lifecycle: plan, approve, apply, verify, roll back.
//!
//! The runtime never upgrades itself. `runtime.upgrade.start(version)` builds a release plan
//! with the migration planner, submits it for approval, and — only once it is approved — asks
//! the apply engine to run it. The apply engine owns the gating, the ordered steps, the
//! journal, verification and the rollback; this module owns **none** of that. It reads the
//! engine's outcome and, when verification failed, returns the engine's restore cause and
//! mechanism as the rollback evidence and marks the operation failed.
//!
//! `runtime.upgrade.resume(version)` re-plans and asks the engine to recover from its journal,
//! so a runtime that was killed mid-upgrade finishes (or rolls back) on the next start instead
//! of waiting for a person.
//!
//! Three seams keep the module testable and honest:
//!
//! - [`ReleasePlanner`] produces the [`ReleasePlan`] (the real adapter calls
//!   `spot_migration_plan::plan`; [`DescriptionPlanner`] is the reference implementation).
//! - [`ApprovalPort`] submits the plan to the approval flow and reports its decision (the real
//!   adapter wraps `spot-migration-approval`).
//! - [`ApplyPort`] invokes the apply engine and hands back its [`ApplyOutcome`] (the real
//!   adapter wraps `spot-migration-apply`).
//!
//! Time is an argument everywhere.

use spot_migration_apply::{
    ApplyOutcome, RecoveryAction, RecoveryOutcome, RestoreCause, RestoreMechanism,
};
use spot_migration_approval::PlanDigest;
use spot_migration_plan::{ReleasePlan, SchemaPlan, SystemDescription};

use crate::error::{RuntimeError, RuntimeResult};

/// Produces the release plan for a target version.
pub trait ReleasePlanner: Send + Sync {
    /// Builds the plan that moves this node to `target_version`.
    ///
    /// # Errors
    /// A backend error when the two system descriptions cannot be read or diffed.
    fn plan(&self, target_version: &str, now: i64) -> RuntimeResult<ReleasePlan>;
}

/// The approval flow's decision for one plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The plan may be applied; the named approvers approved it (empty for an auto-approved
    /// plan).
    Approved {
        /// The distinct approvers whose approval counted.
        approvers: Vec<String>,
    },
    /// The plan was submitted and is awaiting approvals.
    Pending,
    /// At least one approver rejected the plan.
    Rejected {
        /// The rejecting approvers.
        by: Vec<String>,
    },
}

/// Submits a release plan to approval.
pub trait ApprovalPort: Send + Sync {
    /// Submits `plan` and returns the current decision.
    ///
    /// # Errors
    /// A backend error when the submission cannot be recorded.
    fn submit(&self, plan: &ReleasePlan, now: i64) -> RuntimeResult<ApprovalDecision>;
}

/// Proof that a plan was approved, handed to the apply engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// The approved plan's domain-separated digest.
    pub digest: PlanDigest,
    /// The approvers whose approval counted.
    pub approvers: Vec<String>,
}

/// Invokes the apply engine.
pub trait ApplyPort: Send + Sync {
    /// Runs an approved plan through the engine.
    ///
    /// # Errors
    /// A backend error when the engine cannot run at all. A run that fails verification and
    /// rolls back is **not** an error here: the engine reports it as
    /// [`ApplyOutcome::Restored`].
    fn apply(
        &self,
        plan: &ReleasePlan,
        approval: &Approval,
        now: i64,
    ) -> RuntimeResult<ApplyOutcome>;

    /// Recovers an unfinished run from the engine's journal.
    ///
    /// # Errors
    /// A backend error when the journal cannot be read or the recovery cannot run.
    fn recover(&self, plan: &ReleasePlan, now: i64) -> RuntimeResult<RecoveryOutcome>;
}

/// The engine's rollback evidence, carried back when verification failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackEvidence {
    /// Why the run rolled back, as a stable label.
    pub cause: String,
    /// How the rollback was achieved, as a stable label.
    pub mechanism: String,
}

/// What one upgrade operation produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// Every step ran and verification passed.
    Upgraded,
    /// The plan was submitted and is awaiting approval.
    PendingApproval,
    /// The plan was rejected by an approver.
    Rejected {
        /// The rejecting approvers.
        by: Vec<String>,
    },
    /// Verification failed and the engine restored the old version.
    RolledBack {
        /// The engine's rollback evidence.
        evidence: RollbackEvidence,
    },
    /// An interrupted run was recovered from the journal.
    Resumed {
        /// What the engine did, as a stable label.
        action: String,
        /// The engine's state afterwards, as a stable label.
        state: String,
    },
}

impl UpgradeOutcome {
    /// Whether the operation should be reported to the console as a failure.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Rejected { .. } | Self::RolledBack { .. })
    }

    /// A short, secret-free sentence describing the outcome.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Upgraded => "the upgrade was applied and verified".to_owned(),
            Self::PendingApproval => {
                "the release plan was submitted and awaits approval".to_owned()
            }
            Self::Rejected { by } => format!("the release plan was rejected by {}", by.join(", ")),
            Self::RolledBack { evidence } => format!(
                "verification failed and the engine restored the old version via {} ({})",
                evidence.mechanism, evidence.cause
            ),
            Self::Resumed { action, state } => {
                format!("the engine recovered the interrupted run: {action} ({state})")
            }
        }
    }
}

/// What an upgrade operation returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeReport {
    /// The target version string, as requested.
    pub version: String,
    /// The release plan's domain-separated digest, hex.
    pub plan_digest: String,
    /// What happened.
    pub outcome: UpgradeOutcome,
}

/// Starts an upgrade: plan, submit, and apply once approved.
///
/// # Errors
/// The planner, approval or apply port's error; never a rollback, which is reported as
/// [`UpgradeOutcome::RolledBack`] so the caller keeps the evidence.
pub fn start_upgrade(
    planner: &dyn ReleasePlanner,
    approval: &dyn ApprovalPort,
    apply: &dyn ApplyPort,
    version: &str,
    now: i64,
) -> RuntimeResult<UpgradeReport> {
    let plan = planner.plan(version, now)?;
    let digest = PlanDigest::of(&plan);
    let decision = approval.submit(&plan, now)?;
    let outcome = match decision {
        ApprovalDecision::Rejected { by } => UpgradeOutcome::Rejected { by },
        ApprovalDecision::Pending => UpgradeOutcome::PendingApproval,
        ApprovalDecision::Approved { approvers } => {
            let approval = Approval {
                digest: digest.clone(),
                approvers,
            };
            match apply.apply(&plan, &approval, now)? {
                ApplyOutcome::Opened { verified: true, .. } => UpgradeOutcome::Upgraded,
                ApplyOutcome::Opened {
                    verified: false, ..
                } => {
                    return Err(RuntimeError::UpgradeFailed {
                        detail: "the engine opened the site on an unverified version".to_owned(),
                    });
                }
                ApplyOutcome::Restored { cause, mechanism } => UpgradeOutcome::RolledBack {
                    evidence: RollbackEvidence {
                        cause: cause_label(&cause).to_owned(),
                        mechanism: mechanism.label().to_owned(),
                    },
                },
            }
        }
    };
    Ok(UpgradeReport {
        version: version.to_owned(),
        plan_digest: digest.as_hex().to_owned(),
        outcome,
    })
}

/// Resumes an interrupted upgrade from the engine's journal.
///
/// # Errors
/// The planner or apply port's error.
pub fn resume_upgrade(
    planner: &dyn ReleasePlanner,
    apply: &dyn ApplyPort,
    version: &str,
    now: i64,
) -> RuntimeResult<UpgradeReport> {
    let plan = planner.plan(version, now)?;
    let digest = PlanDigest::of(&plan);
    let recovery = apply.recover(&plan, now)?;
    Ok(UpgradeReport {
        version: version.to_owned(),
        plan_digest: digest.as_hex().to_owned(),
        outcome: UpgradeOutcome::Resumed {
            action: recovery_action_label(&recovery.action),
            state: recovery.state.label().to_owned(),
        },
    })
}

/// A stable label for why a run rolled back.
#[must_use]
pub fn cause_label(cause: &RestoreCause) -> &'static str {
    match cause {
        RestoreCause::StepFailed { .. } => "step_failed",
        RestoreCause::VerifyFailed => "verify_failed",
        RestoreCause::Recovery => "recovery",
    }
}

/// A stable label for what a recovery did, taken from the engine's own outcome.
#[must_use]
pub fn recovery_action_label(action: &RecoveryAction) -> String {
    match action {
        RecoveryAction::NotStarted => "not_started".to_owned(),
        RecoveryAction::Resumed { .. } => "resumed".to_owned(),
        RecoveryAction::Restored => "restored".to_owned(),
        RecoveryAction::AlreadyOpen { .. } => "already_open".to_owned(),
        RecoveryAction::Halted => "halted".to_owned(),
    }
}

/// The reference planner: diff two [`SystemDescription`]s with the migration planner.
///
/// The real host builds the two descriptions from the installed apps and their effective
/// metadata; this type is what lets a test (or an offline rehearsal) supply them directly. It
/// contains no upgrade logic: it calls `spot_migration_plan::plan`.
pub struct DescriptionPlanner {
    /// The system as it is now.
    pub from: SystemDescription,
    /// The system as it should be after the upgrade.
    pub to: SystemDescription,
    /// The schema part of the diff (mapped from the backend's planner by the host).
    pub schema: SchemaPlan,
}

impl DescriptionPlanner {
    /// A planner over two descriptions with no schema changes.
    #[must_use]
    pub fn new(from: SystemDescription, to: SystemDescription) -> Self {
        Self {
            from,
            to,
            schema: SchemaPlan::default(),
        }
    }

    /// The same planner with a schema diff.
    #[must_use]
    pub fn with_schema(mut self, schema: SchemaPlan) -> Self {
        self.schema = schema;
        self
    }
}

impl ReleasePlanner for DescriptionPlanner {
    fn plan(&self, target_version: &str, _now: i64) -> RuntimeResult<ReleasePlan> {
        let mut to = self.to.clone();
        if let Ok(version) = target_version.parse::<u64>() {
            to.meta_version = version;
        }
        Ok(spot_migration_plan::plan(&self.from, &to, &self.schema))
    }
}

/// The mechanism a rollback used, for callers that hold only the label.
#[must_use]
pub fn mechanism_label(mechanism: RestoreMechanism) -> &'static str {
    mechanism.label()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use spot_migration_apply::{ApplyState, RecoveryAction, Version};
    use spot_migration_plan::SystemDescription;

    use super::*;

    /// A planner that always returns the same (empty) plan.
    struct EmptyPlanner {
        calls: Mutex<usize>,
    }

    impl ReleasePlanner for EmptyPlanner {
        fn plan(&self, _target_version: &str, _now: i64) -> RuntimeResult<ReleasePlan> {
            *self.calls.lock().unwrap() += 1;
            DescriptionPlanner::new(SystemDescription::new(), SystemDescription::new()).plan("1", 0)
        }
    }

    /// An approval port that returns a scripted decision.
    struct ScriptedApproval {
        decision: ApprovalDecision,
        submitted: Mutex<Vec<String>>,
    }

    impl ScriptedApproval {
        fn new(decision: ApprovalDecision) -> Self {
            Self {
                decision,
                submitted: Mutex::new(Vec::new()),
            }
        }
    }

    impl ApprovalPort for ScriptedApproval {
        fn submit(&self, plan: &ReleasePlan, _now: i64) -> RuntimeResult<ApprovalDecision> {
            self.submitted
                .lock()
                .unwrap()
                .push(plan.plan_digest.clone());
            Ok(self.decision.clone())
        }
    }

    /// An apply port with a scripted outcome and recovery.
    struct ScriptedApply {
        outcome: ApplyOutcome,
        recovery: RecoveryAction,
        applied: Mutex<usize>,
        recovered: Mutex<usize>,
    }

    impl ScriptedApply {
        fn new(outcome: ApplyOutcome) -> Self {
            Self {
                outcome,
                recovery: RecoveryAction::Resumed { from_step: 3 },
                applied: Mutex::new(0),
                recovered: Mutex::new(0),
            }
        }
    }

    impl ApplyPort for ScriptedApply {
        fn apply(
            &self,
            _plan: &ReleasePlan,
            _approval: &Approval,
            _now: i64,
        ) -> RuntimeResult<ApplyOutcome> {
            *self.applied.lock().unwrap() += 1;
            Ok(self.outcome.clone())
        }

        fn recover(&self, _plan: &ReleasePlan, _now: i64) -> RuntimeResult<RecoveryOutcome> {
            *self.recovered.lock().unwrap() += 1;
            Ok(RecoveryOutcome {
                action: self.recovery.clone(),
                state: ApplyState::Open {
                    version: Version::New,
                    verified: true,
                },
            })
        }
    }

    fn planner() -> EmptyPlanner {
        EmptyPlanner {
            calls: Mutex::new(0),
        }
    }

    #[test]
    fn a_verified_upgrade_succeeds() {
        let planner = planner();
        let approval = ScriptedApproval::new(ApprovalDecision::Approved {
            approvers: vec!["alice".to_owned()],
        });
        let apply = ScriptedApply::new(ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        });
        let report = start_upgrade(&planner, &approval, &apply, "2", 1_000).unwrap();
        assert_eq!(report.outcome, UpgradeOutcome::Upgraded);
        assert!(!report.plan_digest.is_empty());
        assert_eq!(*apply.applied.lock().unwrap(), 1);
    }

    #[test]
    fn a_verify_failure_carries_the_rollback_evidence() {
        let planner = planner();
        let approval = ScriptedApproval::new(ApprovalDecision::Approved {
            approvers: Vec::new(),
        });
        let apply = ScriptedApply::new(ApplyOutcome::Restored {
            cause: RestoreCause::VerifyFailed,
            mechanism: RestoreMechanism::Snapshot,
        });
        let report = start_upgrade(&planner, &approval, &apply, "2", 1_000).unwrap();
        let UpgradeOutcome::RolledBack { evidence } = &report.outcome else {
            panic!("expected a rollback, got {:?}", report.outcome);
        };
        assert_eq!(evidence.cause, "verify_failed");
        assert_eq!(evidence.mechanism, "snapshot");
        assert!(report.outcome.is_failure());
        assert!(
            report
                .outcome
                .describe()
                .contains("restored the old version")
        );
    }

    #[test]
    fn a_pending_plan_is_submitted_and_not_applied() {
        let planner = planner();
        let approval = ScriptedApproval::new(ApprovalDecision::Pending);
        let apply = ScriptedApply::new(ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        });
        let report = start_upgrade(&planner, &approval, &apply, "2", 1_000).unwrap();
        assert_eq!(report.outcome, UpgradeOutcome::PendingApproval);
        assert_eq!(*apply.applied.lock().unwrap(), 0);
        assert_eq!(approval.submitted.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_rejected_plan_is_not_applied() {
        let planner = planner();
        let approval = ScriptedApproval::new(ApprovalDecision::Rejected {
            by: vec!["bob".to_owned()],
        });
        let apply = ScriptedApply::new(ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        });
        let report = start_upgrade(&planner, &approval, &apply, "2", 1_000).unwrap();
        assert_eq!(
            report.outcome,
            UpgradeOutcome::Rejected {
                by: vec!["bob".to_owned()]
            }
        );
        assert_eq!(*apply.applied.lock().unwrap(), 0);
    }

    #[test]
    fn a_kill_mid_upgrade_resumes_from_the_journal() {
        let planner = planner();
        let apply = ScriptedApply::new(ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        });
        let report = resume_upgrade(&planner, &apply, "2", 1_000).unwrap();
        assert_eq!(
            report.outcome,
            UpgradeOutcome::Resumed {
                action: "resumed".to_owned(),
                state: "open".to_owned()
            }
        );
        assert_eq!(*apply.recovered.lock().unwrap(), 1);
    }
}
