//! Shared harness for the pipeline's integration tests.
//!
//! It builds a two-record world, a sealed plan and every fake port, and exposes `run`, `recover`
//! and `undo` so a test never repeats the wiring.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs, dead_code)]

use std::collections::BTreeMap;

use spot_agent_tiers::Tier;
use spot_fix_pipeline::testing::{
    FakeApproval, FakeExecutor, FakePreImages, FakeRehearsal, FakeSnapshots, FakeVerifier,
    FakeWorld,
};
use spot_fix_pipeline::{
    Faults, FixContext, FixEngine, FixId, FixJournal, FixOutcome, FixPlan, FixPolicy, FixPorts,
    FixSource, FixStep, InMemoryJournal, MemoryFixAudit, Result, Risk, StructuralKind,
};
use spot_remedies::RemedyId;

/// The remedy id the tests use.
pub const REMEDY: &str = "job.retry_failed";

/// A harness over two record targets.
pub struct Harness {
    /// The fake world.
    pub world: FakeWorld,
    /// The sealed plan.
    pub plan: FixPlan,
    /// The run context.
    pub ctx: FixContext,
    /// The policy.
    pub policy: FixPolicy,
    /// The state before the run.
    pub initial: BTreeMap<String, i128>,
    /// The journal.
    pub journal: InMemoryJournal,
    /// The audit sink.
    pub audit: MemoryFixAudit,
    /// The pre-image store.
    pub pre: FakePreImages,
    /// The snapshot port.
    pub snaps: FakeSnapshots,
    /// The executor.
    pub exec: FakeExecutor,
    /// The verifier.
    pub verifier: FakeVerifier,
    /// The rehearsal.
    pub rehearsal: FakeRehearsal,
    /// The approval port.
    pub approval: FakeApproval,
}

/// How the fake verifier and rehearsal behave.
pub struct FakeOptions {
    /// Whether the invariants hold.
    pub invariants_pass: bool,
    /// Whether the remedy-specific check passes.
    pub remedy_verified: bool,
    /// Whether the symptom cleared.
    pub symptom_cleared: bool,
    /// Whether the rehearsal passes.
    pub rehearsal_pass: bool,
    /// The approval answer.
    pub approval: FakeApproval,
}

impl Default for FakeOptions {
    fn default() -> Self {
        Self {
            invariants_pass: true,
            remedy_verified: true,
            symptom_cleared: true,
            rehearsal_pass: true,
            approval: FakeApproval::approving("alice"),
        }
    }
}

impl FakeOptions {
    /// Options whose symptom re-check fails.
    pub fn symptom_fails() -> Self {
        Self {
            symptom_cleared: false,
            ..Self::default()
        }
    }

    /// Options whose rehearsals fail.
    pub fn rehearsal_fails() -> Self {
        Self {
            rehearsal_pass: false,
            ..Self::default()
        }
    }

    /// Options whose invariants fail.
    pub fn invariants_fail() -> Self {
        Self {
            invariants_pass: false,
            ..Self::default()
        }
    }
}

impl Harness {
    /// A harness over `targets`, with the given options.
    pub fn with_options(
        targets: &[(&str, i128)],
        tier: Tier,
        policy: FixPolicy,
        options: FakeOptions,
    ) -> Self {
        let world = FakeWorld::with(targets);
        let names: Vec<String> = targets.iter().map(|(name, _)| (*name).to_owned()).collect();
        let plan = plan_for(&names, tier);
        let ctx = spot_fix_pipeline::testing::context("site-1", tier, false, 1_000);
        Self {
            pre: FakePreImages::new(&world),
            snaps: FakeSnapshots::new(&world),
            exec: FakeExecutor::new(&world, 1),
            verifier: FakeVerifier {
                world: world.clone(),
                invariants_pass: options.invariants_pass,
                remedy_verified: options.remedy_verified,
                symptom_cleared: options.symptom_cleared,
            },
            rehearsal: FakeRehearsal {
                world: world.clone(),
                pass: options.rehearsal_pass,
            },
            approval: options.approval,
            initial: targets
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect(),
            world,
            plan,
            ctx,
            policy,
            journal: InMemoryJournal::new(),
            audit: MemoryFixAudit::new(),
        }
    }

    /// The default small-fix harness: T1, direct apply, everything passes.
    pub fn small() -> Self {
        let world = FakeWorld::with(&[("job:1", 10), ("job:2", 20)]);
        let verifier = FakeVerifier::passing(&world);
        let rehearsal = FakeRehearsal::passing(&world);
        Self {
            pre: FakePreImages::new(&world),
            snaps: FakeSnapshots::new(&world),
            exec: FakeExecutor::new(&world, 1),
            verifier,
            rehearsal,
            approval: FakeApproval::approving("alice"),
            initial: BTreeMap::from([("job:1".to_owned(), 10), ("job:2".to_owned(), 20)]),
            world,
            plan: FixPlan::seal(
                FixSource::Remedy {
                    remedy: RemedyId::new(REMEDY),
                    plan_hash: "remedy-plan".to_owned(),
                },
                Tier::T1,
                vec![
                    FixStep::new(0, "s0", "job:1", "job.retry", "retry the job"),
                    FixStep::new(1, "s1", "job:2", "job.retry", "retry the job"),
                ],
                Risk::Low,
                true,
                Vec::new(),
            )
            .expect("a valid plan"),
            ctx: spot_fix_pipeline::testing::context("site-1", Tier::T1, false, 1_000),
            policy: FixPolicy::default().with_rehearsal_optional(REMEDY),
            journal: InMemoryJournal::new(),
            audit: MemoryFixAudit::new(),
        }
    }

    /// The default T3 harness: structural, non-deep agent, approving inbox.
    pub fn structural() -> Self {
        let world = FakeWorld::with(&[("meta:party", 1), ("meta:item", 2)]);
        let verifier = FakeVerifier::passing(&world);
        let rehearsal = FakeRehearsal::passing(&world);
        Self {
            pre: FakePreImages::new(&world),
            snaps: FakeSnapshots::new(&world),
            exec: FakeExecutor::new(&world, 1),
            verifier,
            rehearsal,
            approval: FakeApproval::approving("alice"),
            initial: BTreeMap::from([("meta:party".to_owned(), 1), ("meta:item".to_owned(), 2)]),
            world,
            plan: FixPlan::seal(
                FixSource::Structural {
                    structural: StructuralKind::CustomisationBundle,
                    plan_hash: "bundle-plan".to_owned(),
                },
                Tier::T3,
                vec![
                    FixStep::new(0, "s0", "meta:party", "customise.update", "update a field"),
                    FixStep::new(1, "s1", "meta:item", "customise.update", "update a field"),
                ],
                Risk::Medium,
                true,
                Vec::new(),
            )
            .expect("a valid plan"),
            ctx: spot_fix_pipeline::testing::context("site-1", Tier::T3, false, 1_000),
            policy: FixPolicy::default(),
            journal: InMemoryJournal::new(),
            audit: MemoryFixAudit::new(),
        }
    }

    /// The pre-state.
    pub fn pre_state(&self) -> BTreeMap<String, i128> {
        self.initial.clone()
    }

    /// The expected post-state: one added per step.
    pub fn post_state(&self) -> BTreeMap<String, i128> {
        let mut state = self.pre_state();
        for step in &self.plan.steps {
            if let Some(value) = state.get_mut(&step.target) {
                *value += 1;
            }
        }
        state
    }

    /// Runs the plan with `faults`.
    ///
    /// # Errors
    ///
    /// The engine's error, including a kill.
    pub fn run(&mut self, fix: &FixId, faults: &dyn Faults) -> Result<FixOutcome> {
        let plan = self.plan.clone();
        let ctx = self.ctx.clone();
        let policy = self.policy.clone();
        let ports = FixPorts {
            pre_images: &mut self.pre,
            snapshots: &mut self.snaps,
            executor: &mut self.exec,
            verifier: &mut self.verifier,
            rehearsal: &mut self.rehearsal,
            approval: &mut self.approval,
        };
        let mut engine = FixEngine::new(policy, &self.journal, &mut self.audit, ports, faults);
        engine.run(fix, &plan, &ctx)
    }

    /// Recovers the fix with no injected fault.
    ///
    /// # Errors
    ///
    /// The engine's error.
    pub fn recover(&mut self, fix: &FixId) -> Result<FixOutcome> {
        self.exec.fail_at = None;
        let ctx = self.ctx.clone();
        let policy = self.policy.clone();
        let ports = FixPorts {
            pre_images: &mut self.pre,
            snapshots: &mut self.snaps,
            executor: &mut self.exec,
            verifier: &mut self.verifier,
            rehearsal: &mut self.rehearsal,
            approval: &mut self.approval,
        };
        let faults = spot_fix_pipeline::NoFaults;
        let mut engine = FixEngine::new(policy, &self.journal, &mut self.audit, ports, &faults);
        engine.recover(fix, &ctx)
    }

    /// A context at `now`.
    pub fn ctx_at(&self, now: i64) -> FixContext {
        let mut ctx = self.ctx.clone();
        ctx.now = now;
        ctx
    }

    /// Undoes the fix at `now`.
    ///
    /// # Errors
    ///
    /// The engine's error.
    pub fn undo(&mut self, fix: &FixId, now: i64) -> Result<FixOutcome> {
        let faults = spot_fix_pipeline::NoFaults;
        self.undo_with(fix, now, &faults)
    }

    /// Undoes the fix at `now` with `faults`.
    ///
    /// # Errors
    ///
    /// The engine's error, including a kill.
    pub fn undo_with(&mut self, fix: &FixId, now: i64, faults: &dyn Faults) -> Result<FixOutcome> {
        let ctx = self.ctx_at(now);
        let policy = self.policy.clone();
        let ports = FixPorts {
            pre_images: &mut self.pre,
            snapshots: &mut self.snaps,
            executor: &mut self.exec,
            verifier: &mut self.verifier,
            rehearsal: &mut self.rehearsal,
            approval: &mut self.approval,
        };
        let mut engine = FixEngine::new(policy, &self.journal, &mut self.audit, ports, faults);
        engine.undo(fix, &ctx, "test undo")
    }

    /// Closes the undo window at `now`.
    ///
    /// # Errors
    ///
    /// The engine's error.
    pub fn close_expired(&mut self, fix: &FixId, now: i64) -> Result<bool> {
        let policy = self.policy.clone();
        let ports = FixPorts {
            pre_images: &mut self.pre,
            snapshots: &mut self.snaps,
            executor: &mut self.exec,
            verifier: &mut self.verifier,
            rehearsal: &mut self.rehearsal,
            approval: &mut self.approval,
        };
        let faults = spot_fix_pipeline::NoFaults;
        let mut engine = FixEngine::new(policy, &self.journal, &mut self.audit, ports, &faults);
        engine.close_expired(fix, now)
    }

    /// The journaled progress of `fix`.
    pub fn progress(&self, fix: &FixId) -> spot_fix_pipeline::Progress {
        let entries = self.journal.entries(fix).expect("the journal reads");
        spot_fix_pipeline::Progress::fold(fix, entries)
    }

    /// The journal records of `fix`.
    pub fn records(&self, fix: &FixId) -> Vec<spot_fix_pipeline::JournalRecord> {
        self.journal
            .entries(fix)
            .expect("the journal reads")
            .into_iter()
            .map(|entry| entry.record)
            .collect()
    }
}

/// A sealed plan over `targets`.
pub fn plan_for(targets: &[String], tier: Tier) -> FixPlan {
    let steps = targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            FixStep::new(
                index,
                &format!("s{index}"),
                target,
                "job.retry",
                "retry the job",
            )
        })
        .collect();
    let (source, tier) = if tier == Tier::T3 {
        (
            FixSource::Structural {
                structural: StructuralKind::Migration,
                plan_hash: "structural-plan".to_owned(),
            },
            Tier::T3,
        )
    } else {
        (
            FixSource::Remedy {
                remedy: RemedyId::new(REMEDY),
                plan_hash: "remedy-plan".to_owned(),
            },
            tier,
        )
    };
    FixPlan::seal(source, tier, steps, Risk::Low, true, Vec::new()).expect("a valid plan")
}

/// A fix id for a test.
pub fn fix_id() -> FixId {
    FixId::new("fix-test-1").expect("a valid fix id")
}

/// The journal record labels, in order, for an ordering assertion.
pub fn labels(records: &[spot_fix_pipeline::JournalRecord]) -> Vec<&'static str> {
    records
        .iter()
        .map(|record| match record {
            spot_fix_pipeline::JournalRecord::Proposed { .. } => "proposed",
            spot_fix_pipeline::JournalRecord::Planned { .. } => "planned",
            spot_fix_pipeline::JournalRecord::ApprovalRequested { .. } => "approval_requested",
            spot_fix_pipeline::JournalRecord::Approved { .. } => "approved",
            spot_fix_pipeline::JournalRecord::SelfApproved { .. } => "self_approved",
            spot_fix_pipeline::JournalRecord::ApprovalRefused { .. } => "approval_refused",
            spot_fix_pipeline::JournalRecord::RehearsalStarted { .. } => "rehearsal_started",
            spot_fix_pipeline::JournalRecord::Rehearsed { .. } => "rehearsed",
            spot_fix_pipeline::JournalRecord::RehearsalFailed { .. } => "rehearsal_failed",
            spot_fix_pipeline::JournalRecord::ApplyStarted { .. } => "apply_started",
            spot_fix_pipeline::JournalRecord::SnapshotTaken { .. } => "snapshot_taken",
            spot_fix_pipeline::JournalRecord::PreImagesCaptured { .. } => "pre_images_captured",
            spot_fix_pipeline::JournalRecord::PreFingerprintCaptured { .. } => {
                "pre_fingerprint_captured"
            }
            spot_fix_pipeline::JournalRecord::StepStarted { .. } => "step_started",
            spot_fix_pipeline::JournalRecord::StepApplied { .. } => "step_applied",
            spot_fix_pipeline::JournalRecord::StepFailed { .. } => "step_failed",
            spot_fix_pipeline::JournalRecord::Applied { .. } => "applied",
            spot_fix_pipeline::JournalRecord::VerifyStarted => "verify_started",
            spot_fix_pipeline::JournalRecord::Verified { .. } => "verified",
            spot_fix_pipeline::JournalRecord::VerifyFailed { .. } => "verify_failed",
            spot_fix_pipeline::JournalRecord::RollbackStarted { .. } => "rollback_started",
            spot_fix_pipeline::JournalRecord::RolledBack { .. } => "rolled_back",
            spot_fix_pipeline::JournalRecord::RollbackFailed { .. } => "rollback_failed",
            spot_fix_pipeline::JournalRecord::ResidueChecked { .. } => "residue_checked",
            spot_fix_pipeline::JournalRecord::Failed { .. } => "failed",
            spot_fix_pipeline::JournalRecord::UndoWindowOpened { .. } => "undo_window_opened",
            spot_fix_pipeline::JournalRecord::UndoWindowClosed { .. } => "undo_window_closed",
            spot_fix_pipeline::JournalRecord::UndoStarted { .. } => "undo_started",
            spot_fix_pipeline::JournalRecord::Undone { .. } => "undone",
            spot_fix_pipeline::JournalRecord::UndoRefused { .. } => "undo_refused",
        })
        .collect()
}
