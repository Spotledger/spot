//! Recorded security findings: silence is unknown, the approval checks live in the core, and a
//! rehearsal report cannot be forged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::Arc;

use spot_desired_state::{
    Approval, ApprovalDecision, ApprovalRules, ApprovedPlan, BulkPlan, BulkRun, DesiredFleet,
    DriftSubject, ExemptionBook, FleetRollout, InMemoryObserved, Liveness, Moment, NodeId,
    ObservedSnapshot, PlanStep, RemediationPolicy, Rollout, RunAction, RunGuards, StepFailure,
    StepRunner, detect_drift, detect_unexempted, liveness, remediate,
};
use support::{
    DEADLINE, NOW, REQUESTER, fleet, group_state, guards, observed, plan_for, registry,
    rollout_plan, rules, seed, servers,
};

fn desired(members: &[NodeId]) -> DesiredFleet {
    DesiredFleet::new(registry(), vec![group_state(members)]).unwrap()
}

// ---- Recorded security finding: a silent server is unknown, never healthy ----------------------------------

#[test]
fn a_server_never_seen_is_unknown() {
    let members = servers(2);
    let mut reports = observed(&members, &BTreeSet::new());
    reports.forget(&members[1]);
    let found = detect_drift(&desired(&members), &reports, NOW, DEADLINE);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].server, members[1]);
    assert_eq!(found[0].subject, DriftSubject::Silent);
    assert!(found[0].is_silent());
    assert_eq!(found[0].silent_since(), None);
    assert!(found[0].describe().contains("never seen"));
}

#[test]
fn a_server_silent_beyond_the_deadline_is_unknown_with_its_last_seen_time() {
    let members = servers(1);
    let reports = observed(&members, &BTreeSet::new());
    // seen at 1_000; at exactly the deadline it is still fresh, one microsecond later it is not
    let fresh = detect_drift(&desired(&members), &reports, NOW + DEADLINE, DEADLINE);
    assert!(fresh.is_empty());
    let late = detect_drift(&desired(&members), &reports, NOW + DEADLINE + 1, DEADLINE);
    assert_eq!(late.len(), 1);
    assert!(late[0].is_silent());
    assert_eq!(late[0].silent_since(), Some(NOW));
}

#[test]
fn a_stale_report_is_not_compared_so_it_cannot_read_as_in_line() {
    let members = servers(1);
    // the stale report says "warn" (in line); the server has gone quiet since
    let reports = observed(&members, &BTreeSet::new());
    let found = detect_drift(&desired(&members), &reports, NOW + DEADLINE + 1, DEADLINE);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].subject, DriftSubject::Silent);
}

#[test]
fn a_report_from_the_future_or_without_a_snapshot_is_silent() {
    let members = servers(2);
    let mut reports = InMemoryObserved::new();
    // an agent claiming a report time after the hub's now cannot be trusted
    reports.record_seen(members[0].clone(), ObservedSnapshot::new(NOW), NOW + 5);
    // a heartbeat without any snapshot proves nothing about the state
    reports.record_heartbeat(members[1].clone(), NOW);
    assert!(liveness(&reports, &members[0], NOW, DEADLINE).is_silent());
    assert_eq!(
        liveness(&reports, &members[1], NOW, DEADLINE),
        Liveness::Silent { since: Some(NOW) }
    );
    let found = detect_drift(&desired(&members), &reports, NOW, DEADLINE);
    assert_eq!(found.iter().filter(|drift| drift.is_silent()).count(), 2);
}

#[test]
fn a_negative_deadline_is_zero_not_unlimited() {
    let members = servers(1);
    let reports = observed(&members, &BTreeSet::new());
    assert!(!liveness(&reports, &members[0], NOW, -5).is_silent());
    assert!(liveness(&reports, &members[0], NOW + 1, -5).is_silent());
}

#[test]
fn silence_cannot_be_exempted_and_is_never_remediated() {
    let members = servers(1);
    let reports = InMemoryObserved::new();
    let found = detect_unexempted(
        &desired(&members),
        &reports,
        &ExemptionBook::new(),
        NOW,
        DEADLINE,
    );
    assert_eq!(found.len(), 1);
    let plan = remediate(&found, &RemediationPolicy::standard());
    assert!(plan.is_empty(), "a silent server has nothing to apply");
    assert_eq!(plan.skipped.len(), 1);
}

/// Records which servers were applied to.
struct Recording {
    applied: RefCell<Vec<NodeId>>,
}

impl Recording {
    fn new() -> Self {
        Self {
            applied: RefCell::new(Vec::new()),
        }
    }
}

impl StepRunner for Recording {
    fn apply(&self, server: &NodeId, _step: &PlanStep) -> Result<(), StepFailure> {
        self.applied.borrow_mut().push(server.clone());
        Ok(())
    }

    fn verify(&self, _server: &NodeId, _step: &PlanStep) -> bool {
        true
    }
}

fn start(plan: &BulkPlan, members: &[NodeId], guards: RunGuards, wall: i64) -> (BulkRun, NodeId) {
    let mut approvals = spot_desired_state::MemoryApprovals::new();
    approvals.approve(plan, ["alice".to_owned()], NOW);
    let proof = ApprovedPlan::authorise(&approvals, plan, REQUESTER, &rules(), NOW).unwrap();
    let rehearsal = plan.rehearse();
    let rollout = FleetRollout::new(rollout_plan(), &fleet(members), seed()).unwrap();
    let canary = rollout.schedule()[0].nodes[0].clone();
    let run = BulkRun::start(
        plan.clone(),
        &proof,
        &rehearsal,
        Box::new(rollout),
        guards,
        Moment::ZERO,
        wall,
    )
    .unwrap();
    (run, canary)
}

fn run_to_end(run: &mut BulkRun, runner: &dyn StepRunner, wall: i64) -> RunAction {
    for tick in 0..60 {
        let action = run.tick(runner, Moment::from_secs(tick), wall);
        if matches!(
            action,
            RunAction::Halted { .. } | RunAction::Complete { .. } | RunAction::Aborted { .. }
        ) {
            return action;
        }
    }
    panic!("the run did not settle");
}

#[test]
fn a_silent_canary_blocks_the_rollout_and_nothing_is_applied() {
    let members = servers(10);
    let plan = plan_for(&members);
    // find the canary first, then build guards where it never reported
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let canary = rollout.schedule()[0].nodes[0].clone();
    let mut reports = observed(&members, &BTreeSet::new());
    reports.forget(&canary);
    let guards = RunGuards::new(Arc::new(reports), DEADLINE, rules());

    let (mut run, _) = start(&plan, &members, guards, NOW);
    let runner = Recording::new();
    let action = run_to_end(&mut run, &runner, NOW);
    assert!(matches!(action, RunAction::Halted { .. }), "{action:?}");
    assert!(
        runner.applied.borrow().is_empty(),
        "no server was applied to"
    );
    assert!(run.verified_servers().is_empty());
    assert_eq!(run.failures().len(), 1);
    assert_eq!(run.failures()[0].node, canary);
}

#[test]
fn a_server_that_goes_silent_mid_rollout_is_not_applied_to() {
    let members = servers(10);
    let plan = plan_for(&members);
    // every server was last seen at NOW; the run's clock moves past the deadline
    let (mut run, _) = start(&plan, &members, guards(&members), NOW);
    let runner = Recording::new();
    let action = run_to_end(&mut run, &runner, NOW + DEADLINE + 1);
    assert!(matches!(action, RunAction::Halted { .. }), "{action:?}");
    assert!(runner.applied.borrow().is_empty());
}

#[test]
fn a_fleet_that_keeps_reporting_still_completes() {
    let members = servers(10);
    let plan = plan_for(&members);
    let (mut run, _) = start(&plan, &members, guards(&members), NOW);
    let runner = Recording::new();
    let action = run_to_end(&mut run, &runner, NOW);
    assert!(matches!(action, RunAction::Complete { .. }), "{action:?}");
    assert_eq!(runner.applied.borrow().len(), 10);
}

// ---- Recorded security finding: the approval checks run in the core, for any adapter -----------------------

/// An adapter that checks nothing: it approves every plan as the names it was given, whenever
/// it was told.
struct Lax {
    approvers: Vec<String>,
    decided_at: i64,
}

impl Approval for Lax {
    fn decide(&self, _plan: &BulkPlan, _now: i64) -> ApprovalDecision {
        ApprovalDecision::Approved {
            approvers: self.approvers.clone(),
            decided_at: self.decided_at,
        }
    }
}

fn lax(approvers: &[&str], decided_at: i64) -> Lax {
    Lax {
        approvers: approvers.iter().map(|name| (*name).to_owned()).collect(),
        decided_at,
    }
}

fn refusal(adapter: &Lax, rules: &ApprovalRules, now: i64) -> String {
    let plan = plan_for(&servers(2));
    let error = ApprovedPlan::authorise(adapter, &plan, REQUESTER, rules, now).unwrap_err();
    assert_eq!(error.code(), "desired_state.not_approved");
    error.to_string()
}

#[test]
fn the_requester_cannot_approve_their_own_plan_with_any_adapter() {
    let message = refusal(&lax(&["alice", " Requester "], NOW), &rules(), NOW);
    assert!(message.contains("requester"), "{message}");
}

#[test]
fn a_stale_or_future_dated_approval_is_refused_in_the_core() {
    let rules = ApprovalRules::new(100, 1);
    let stale = refusal(&lax(&["alice"], NOW), &rules, NOW + 101);
    assert!(stale.contains("too old"), "{stale}");
    let future = refusal(&lax(&["alice"], NOW + 1), &rules, NOW);
    assert!(future.contains("future"), "{future}");
    let plan = plan_for(&servers(2));
    assert!(
        ApprovedPlan::authorise(&lax(&["alice"], NOW), &plan, REQUESTER, &rules, NOW + 100).is_ok()
    );
}

#[test]
fn one_person_listed_twice_is_one_approver() {
    let rules = ApprovalRules::new(1_000, 2);
    let message = refusal(&lax(&["alice", "ALICE"], NOW), &rules, NOW);
    assert!(message.contains("too few"), "{message}");
    let plan = plan_for(&servers(2));
    let proof =
        ApprovedPlan::authorise(&lax(&["alice", "bob"], NOW), &plan, REQUESTER, &rules, NOW)
            .unwrap();
    assert_eq!(proof.approvers(), ["alice", "bob"]);
    assert_eq!(proof.approved_at(), NOW);
}

#[test]
fn a_blank_approver_or_an_empty_list_is_refused() {
    let rules = rules();
    assert!(refusal(&lax(&["alice", "  "], NOW), &rules, NOW).contains("blank"));
    assert!(refusal(&lax(&[], NOW), &rules, NOW).contains("too few"));
}

#[test]
fn the_rules_only_tighten() {
    let rules = ApprovalRules::new(-5, 0);
    assert_eq!(rules.max_age(), 0);
    assert_eq!(rules.min_approvers(), 1);
}

#[test]
fn an_approval_that_went_stale_before_the_run_starts_is_refused() {
    let members = servers(3);
    let plan = plan_for(&members);
    let mut approvals = spot_desired_state::MemoryApprovals::new();
    approvals.approve(&plan, ["alice".to_owned()], NOW);
    let proof = ApprovedPlan::authorise(&approvals, &plan, REQUESTER, &rules(), NOW).unwrap();
    let rehearsal = plan.rehearse();
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let late = NOW + rules().max_age() + 1;
    let error = BulkRun::start(
        plan,
        &proof,
        &rehearsal,
        Box::new(rollout),
        guards(&members),
        Moment::ZERO,
        late,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.not_approved");
}

// ---- Recorded security finding: a rehearsal report comes only from the rehearsal ---------------------------

#[test]
fn a_report_for_another_plan_is_refused() {
    let members = servers(3);
    let plan = plan_for(&members);
    let other = plan_for(&servers(4));
    let mut approvals = spot_desired_state::MemoryApprovals::new();
    approvals.approve(&plan, ["alice".to_owned()], NOW);
    let proof = ApprovedPlan::authorise(&approvals, &plan, REQUESTER, &rules(), NOW).unwrap();
    let report = other.rehearse();
    assert!(
        report.passed(),
        "the report is genuine, just for another plan"
    );
    assert!(!report.covers(&plan));
    let rollout = FleetRollout::new(rollout_plan(), &fleet(&members), seed()).unwrap();
    let error = BulkRun::start(
        plan,
        &proof,
        &report,
        Box::new(rollout),
        guards(&members),
        Moment::ZERO,
        NOW,
    )
    .unwrap_err();
    assert_eq!(error.code(), "desired_state.rehearsal_failed");
}

#[test]
fn the_pass_state_is_derived_from_the_rehearsal() {
    let mut empty = plan_for(&servers(2));
    empty.steps.clear();
    assert!(!empty.rehearse().passed());
    let mut broken = plan_for(&servers(2));
    broken.steps[1].operations.clear();
    let report = broken.rehearse();
    assert!(!report.passed());
    assert_eq!(report.problems().len(), 1);
    assert_eq!(report.checked_steps(), 2);
    assert!(report.covers(&broken));
    assert!(plan_for(&servers(2)).rehearse().passed());
}
