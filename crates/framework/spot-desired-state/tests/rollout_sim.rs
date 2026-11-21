//! The bulk rollout acceptance: 100 servers, canary first, halt on failure, explicit resume.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;

use spot_desired_state::{
    ApprovalDecision, ApprovedPlan, BulkRun, Error, FleetRollout, MemoryApprovals, Moment, NodeId,
    PauseReason, PlanStep, RehearsalReport, RemediationPolicy, Rollout, RunAction, StepFailure,
    StepRunner, remediate,
};
use support::{fleet, plan_for, rollout_plan, seed, servers};

/// A runner that applies everything and verifies everything except the servers it is told to
/// fail.
struct FailingVerify {
    failing: RefCell<BTreeSet<NodeId>>,
}

impl FailingVerify {
    fn new(failing: impl IntoIterator<Item = NodeId>) -> Self {
        Self {
            failing: RefCell::new(failing.into_iter().collect()),
        }
    }

    fn allow(&self, server: &NodeId) {
        self.failing.borrow_mut().remove(server);
    }
}

impl StepRunner for FailingVerify {
    fn apply(&self, _server: &NodeId, _step: &PlanStep) -> Result<(), StepFailure> {
        Ok(())
    }

    fn verify(&self, server: &NodeId, _step: &PlanStep) -> bool {
        !self.failing.borrow().contains(server)
    }
}

/// A runner where everything works.
struct AlwaysHealthy;

impl StepRunner for AlwaysHealthy {
    fn apply(&self, _server: &NodeId, _step: &PlanStep) -> Result<(), StepFailure> {
        Ok(())
    }

    fn verify(&self, _server: &NodeId, _step: &PlanStep) -> bool {
        true
    }
}

fn approved(plan: &spot_desired_state::BulkPlan) -> (ApprovedPlan, RehearsalReport) {
    let mut approvals = MemoryApprovals::new();
    approvals.approve(plan, ["alice".to_owned(), "bob".to_owned()], support::NOW);
    let proof = ApprovedPlan::authorise(
        &approvals,
        plan,
        support::REQUESTER,
        &support::rules(),
        support::NOW,
    )
    .unwrap();
    let rehearsal = plan.rehearse();
    assert!(rehearsal.passed());
    (proof, rehearsal)
}

fn terminal(action: &RunAction) -> bool {
    matches!(
        action,
        RunAction::Halted { .. } | RunAction::Complete { .. } | RunAction::Aborted { .. }
    )
}

fn drive(run: &mut BulkRun, runner: &dyn StepRunner, from: u64) -> RunAction {
    let mut action = run.tick(runner, Moment::from_secs(from), support::NOW);
    let mut tick = from.saturating_add(1);
    while !terminal(&action) {
        assert!(tick < from.saturating_add(60), "the run did not settle");
        action = run.tick(runner, Moment::from_secs(tick), support::NOW);
        tick = tick.saturating_add(1);
    }
    action
}

#[test]
fn a_failed_canary_halts_the_fleet_and_no_later_wave_starts() {
    let members = servers(100);
    let plan = remediate(&support::drifts(&members), &RemediationPolicy::standard());
    assert_eq!(plan.size(), 100);
    let (proof, rehearsal) = approved(&plan);

    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let canary = rollout.schedule()[0].nodes[0].clone();
    let runner = FailingVerify::new([canary.clone()]);

    let mut run = BulkRun::start(
        plan,
        &proof,
        &rehearsal,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap();

    let action = drive(&mut run, &runner, 0);
    let RunAction::Halted { pause } = &action else {
        panic!("expected a halt, got {action:?}");
    };
    assert_eq!(pause.reason, PauseReason::CanaryFailure);
    assert_eq!(pause.node.as_ref(), Some(&canary));

    assert_eq!(run.waves_started(), &[0], "no later wave may start");
    assert_eq!(run.applied_servers(), vec![canary]);
    assert_eq!(run.failures().len(), 1);
}

#[test]
fn an_explicit_resume_continues_after_the_canary_is_fixed() {
    let members = servers(100);
    let plan = remediate(&support::drifts(&members), &RemediationPolicy::standard());
    let (proof, rehearsal) = approved(&plan);
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let canary = rollout.schedule()[0].nodes[0].clone();
    let runner = FailingVerify::new([canary.clone()]);
    let mut run = BulkRun::start(
        plan,
        &proof,
        &rehearsal,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap();

    assert!(matches!(
        drive(&mut run, &runner, 0),
        RunAction::Halted { .. }
    ));

    runner.allow(&canary);
    let resumed = run.resume(Moment::from_secs(10)).unwrap();
    assert!(matches!(resumed, RunAction::Resumed { index: 0 }));

    let action = drive(&mut run, &runner, 11);
    let RunAction::Complete { waves, servers } = action else {
        panic!("expected completion after the fix, got {action:?}");
    };
    assert_eq!(waves, 3);
    assert_eq!(servers, 100);
    assert_eq!(run.waves_started(), &[0, 1, 2]);
    assert_eq!(run.verified_servers().len(), 100);
}

#[test]
fn a_healthy_fleet_rolls_out_in_three_waves() {
    let members = servers(100);
    let plan = remediate(&support::drifts(&members), &RemediationPolicy::standard());
    let (proof, rehearsal) = approved(&plan);
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let mut run = BulkRun::start(
        plan,
        &proof,
        &rehearsal,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap();

    let action = drive(&mut run, &AlwaysHealthy, 0);
    assert!(matches!(
        action,
        RunAction::Complete {
            waves: 3,
            servers: 100
        }
    ));
    assert_eq!(run.waves_started(), &[0, 1, 2]);
    assert_eq!(run.applied_servers().len(), 100);
    assert_eq!(run.verified_servers().len(), 100);
}

#[test]
fn a_plan_that_was_not_approved_is_refused() {
    let members = servers(3);
    let plan = plan_for(&members);

    let pending = MemoryApprovals::new();
    let error = ApprovedPlan::authorise(
        &pending,
        &plan,
        support::REQUESTER,
        &support::rules(),
        support::NOW,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.not_approved");

    let mut rejected = MemoryApprovals::new();
    rejected.reject(&plan, ["carol".to_owned()]);
    let error = ApprovedPlan::authorise(
        &rejected,
        &plan,
        support::REQUESTER,
        &support::rules(),
        support::NOW,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.not_approved");
}

#[test]
fn a_stale_approval_never_authorises_a_changed_plan() {
    let members = servers(3);
    let plan = plan_for(&members);
    let (proof, rehearsal) = approved(&plan);

    let mut changed = plan.clone();
    changed.steps[0].operations.clear();
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let error = BulkRun::start(
        changed,
        &proof,
        &rehearsal,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.plan_changed");
}

#[test]
fn a_failed_rehearsal_or_a_server_no_wave_covers_is_refused() {
    let members = servers(3);
    let plan = plan_for(&members);
    let (proof, _) = approved(&plan);

    // A report with a problem can only come from rehearsing a plan that has one.
    let mut broken = plan.clone();
    broken.steps[0].operations.clear();
    let failed = broken.rehearse();
    assert!(!failed.passed());
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let error = BulkRun::start(
        plan.clone(),
        &proof,
        &failed,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.rehearsal_failed");

    let members = servers(5);
    let plan = plan_for(&members);
    let (_, rehearsal) = approved(&plan);
    let mut outside = plan.clone();
    let extra = support::server(99);
    outside.steps.push(spot_desired_state::PlanStep {
        id: "srv-099/6".to_owned(),
        server: extra,
        group: support::GROUP.to_owned(),
        operations: plan.steps[0].operations.clone(),
        risk: plan.risk,
    });
    let mut approvals = MemoryApprovals::new();
    approvals.record(
        &outside,
        ApprovalDecision::Approved {
            approvers: vec!["alice".to_owned()],
            decided_at: support::NOW,
        },
    );
    let outside_proof = ApprovedPlan::authorise(
        &approvals,
        &outside,
        support::REQUESTER,
        &support::rules(),
        support::NOW,
    )
    .unwrap();
    let outside_rehearsal = outside.rehearse();
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&servers(5)), seed()).unwrap();
    let error = BulkRun::start(
        outside,
        &outside_proof,
        &outside_rehearsal,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.unknown_server");
    assert_eq!(rehearsal, plan.rehearse());
}

#[test]
fn an_empty_plan_is_refused() {
    let members = servers(3);
    let mut plan = plan_for(&members);
    plan.steps.clear();
    let mut approvals = MemoryApprovals::new();
    approvals.approve(&plan, ["alice".to_owned()], support::NOW);
    let proof = ApprovedPlan::authorise(
        &approvals,
        &plan,
        support::REQUESTER,
        &support::rules(),
        support::NOW,
    )
    .unwrap();
    let rehearsal = plan.rehearse();
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let error = BulkRun::start(
        plan,
        &proof,
        &rehearsal,
        Box::new(rollout),
        support::guards(&members),
        Moment::ZERO,
        support::NOW,
    )
    .unwrap_err();
    assert!(matches!(error, Error::EmptyPlan));
}
