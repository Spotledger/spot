//! Emergency access with controller review: the firefighter pattern, the second-check policy,
//! the action recording and the escalation timer.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;

use proptest::prelude::*;
use spot_bpm_engine::{InMemoryTimerPort, TimerPort};
use spot_break_glass::{MemoryRecorder, Recorder};
use spot_iam_governance::{
    ApprovalDecision, EscalationTarget, GovError, GrantScope, LaunchReceipt, ProcessLauncher,
};
use spot_iam_questions::{
    EmergencyGrant, EmergencyPhase, EmergencyPolicy, EmergencyRequest, MemoryEmergencyAuditor,
    QuestionsError, ReviewState, SessionContext,
};
use spot_iam_roles::Source;

/// A launcher that records how often it was called.
#[derive(Default)]
struct FakeLauncher {
    launches: Cell<usize>,
}

impl ProcessLauncher for FakeLauncher {
    fn launch(
        &self,
        _process: &spot_bpm_model::ProcessVersion,
        request: &spot_iam_governance::GrantRequestId,
    ) -> Result<LaunchReceipt, GovError> {
        self.launches.set(self.launches.get() + 1);
        Ok(LaunchReceipt {
            case_id: format!("case:{}", request.as_str()),
            process_hash: "hash".to_owned(),
        })
    }
}

/// The decide call for tests that are not about the approver rule: no role lookup, and the
/// refusal rows go to a throwaway auditor.
trait DecidePlain {
    fn decide_plain(
        &mut self,
        decision: ApprovalDecision,
        by: &str,
        at: i64,
    ) -> Result<(), QuestionsError>;
}

impl DecidePlain for EmergencyGrant {
    fn decide_plain(
        &mut self,
        decision: ApprovalDecision,
        by: &str,
        at: i64,
    ) -> Result<(), QuestionsError> {
        self.decide(
            decision,
            by,
            at,
            &|_, _| false,
            &MemoryEmergencyAuditor::new(),
        )
    }
}

fn policy() -> EmergencyPolicy {
    EmergencyPolicy {
        max_duration_secs: 3600,
        approval_sla_secs: 3600,
        review_due_secs: 100,
        escalate_after_secs: 100,
        max_escalations: 2,
        require_second_check_when_approver_reviews: true,
        emergency_roles: vec!["Emergency Admin".to_owned()],
    }
}

fn request(id: &str, duration: u64) -> EmergencyRequest {
    EmergencyRequest::new(
        id,
        "ben",
        "Emergency Admin",
        GrantScope::unscoped(),
        "production incident 4711",
        "cara",
        "dave",
        duration,
    )
    .expect("the request is well formed")
}

fn active_grant(id: &str, launcher: &FakeLauncher) -> EmergencyGrant {
    let mut grant = EmergencyGrant::request(request(id, 600), policy()).expect("policy allows it");
    grant.begin(launcher, 10).expect("launch");
    grant
        .decide_plain(ApprovalDecision::Approve, "cara", 20)
        .expect("the approver may approve");
    grant
}

#[test]
fn the_full_lifecycle_grants_a_time_boxed_assignment_only_after_approval() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-1", 600), policy()).expect("request");
    assert!(matches!(grant.phase, EmergencyPhase::Requested));
    // Nothing is granted before the approval.
    assert!(grant.assignment().is_none());
    assert!(grant.activate(15).is_err(), "activation without approval");

    grant.begin(&launcher, 10).expect("launch");
    assert!(matches!(
        grant.phase,
        EmergencyPhase::AwaitingApproval { .. }
    ));
    assert_eq!(launcher.launches.get(), 1);
    // A second begin is refused: the request is no longer fresh.
    assert!(grant.begin(&launcher, 11).is_err());
    assert_eq!(launcher.launches.get(), 1);

    grant
        .decide_plain(ApprovalDecision::Approve, "cara", 20)
        .expect("approved");
    let assignment = grant.activate(30).expect("activate").clone();
    assert_eq!(assignment.source, Source::Emergency);
    assert_eq!(assignment.user, "ben");
    assert_eq!(assignment.id, "emergency:eg-1");
    assert_eq!(grant.ends_at(), Some(630));
    assert_eq!(
        assignment.scope.restrictions.len(),
        0,
        "an unscoped emergency grant applies everywhere"
    );
}

#[test]
fn the_approver_may_not_be_the_grantee() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-2", 600), policy()).expect("request");
    grant.begin(&launcher, 10).expect("launch");
    let err = grant
        .decide_plain(ApprovalDecision::Approve, "ben", 20)
        .expect_err("the grantee may not approve their own grant");
    assert_eq!(err.code(), spot_iam_questions::codes::APPROVAL);
}

#[test]
fn a_rejected_request_grants_nothing() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-3", 600), policy()).expect("request");
    grant.begin(&launcher, 10).expect("launch");
    grant
        .decide_plain(
            ApprovalDecision::Reject {
                reason: "not justified".to_owned(),
            },
            "cara",
            20,
        )
        .expect("rejected");
    assert!(matches!(grant.phase, EmergencyPhase::Rejected { .. }));
    assert!(grant.activate(30).is_err());
}

#[test]
fn a_duration_above_the_policy_maximum_is_refused() {
    let err =
        EmergencyGrant::request(request("eg-4", 4000), policy()).expect_err("above the maximum");
    assert_eq!(err.code(), spot_iam_questions::codes::EMERGENCY_DURATION);
}

#[test]
fn every_action_is_recorded_and_tagged_with_the_grant() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-5", &launcher);
    grant.activate(30).expect("activate");
    let context = grant.context();
    let mut recorder = MemoryRecorder::new("eg-5", 1 << 20);
    grant
        .record(&context, "read", Some("invoice-1"), 40, &mut recorder)
        .expect("recorded");
    grant
        .record(&context, "post", None, 41, &mut recorder)
        .expect("recorded");
    assert_eq!(grant.actions.len(), 2);
    assert_eq!(grant.actions[0].grant, "eg-5");
    assert_eq!(recorder.frame_count(), 2);
    assert!(!recorder.truncated());

    // An action tagged with another grant is refused.
    let wrong = SessionContext {
        grant: "other".to_owned(),
        user: "ben".to_owned(),
    };
    let err = grant
        .record(&wrong, "read", None, 42, &mut recorder)
        .expect_err("not this grant's context");
    assert_eq!(err.code(), spot_iam_questions::codes::RECORDING);

    // An action after the session's end is refused.
    grant.end(630).expect("end");
    let err = grant
        .record(&context, "read", None, 631, &mut recorder)
        .expect_err("the session ended");
    assert_eq!(err.code(), spot_iam_questions::codes::EMERGENCY_STATE);
}

#[test]
fn a_session_is_never_reviewed_by_its_own_user() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-6", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    let err = grant
        .open_review("ben", 640, &timers)
        .expect_err("the user cannot review their own session");
    assert_eq!(err.code(), spot_iam_questions::codes::REVIEW_SELF);
}

#[test]
fn a_distinct_controller_confirms_and_the_wrong_person_cannot() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-7", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    {
        let review = grant.open_review("erin", 640, &timers).expect("open");
        assert_eq!(review.checks_required, 1, "erin is not the approver");
        assert!(review.is_pending());
    }
    assert!(
        grant.confirm("ben", 641, None).is_err(),
        "the user cannot confirm"
    );
    assert!(
        grant.confirm("frank", 641, None).is_err(),
        "a stranger cannot confirm"
    );
    let review = grant
        .confirm("erin", 642, Some("looks routine"))
        .expect("confirm");
    assert!(matches!(review.state, ReviewState::Confirmed { .. }));
    assert_eq!(review.checks.len(), 1);
}

#[test]
fn flagging_ends_the_review_for_a_person() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-8", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    grant.open_review("erin", 640, &timers).expect("open");
    let review = grant
        .flag("erin", 641, "posted outside the ticket")
        .expect("flag");
    match &review.state {
        ReviewState::Flagged { reason, .. } => assert_eq!(reason, "posted outside the ticket"),
        other => panic!("not flagged: {other:?}"),
    }
}

#[test]
fn a_controller_who_is_the_approver_needs_a_second_distinct_check() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-9", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    // cara approved it, so cara as controller needs a second, distinct check.
    {
        let review = grant.open_review("cara", 640, &timers).expect("open");
        assert_eq!(review.checks_required, 2);
    }
    let after_first = grant.confirm("cara", 641, None).expect("the first check");
    assert!(
        after_first.is_pending(),
        "the review is not complete after one check"
    );
    assert!(
        grant.confirm("cara", 642, None).is_err(),
        "the same person cannot supply both checks"
    );
    let review = grant
        .confirm("erin", 642, Some("second pair of eyes"))
        .expect("the second check");
    assert!(matches!(review.state, ReviewState::Confirmed { .. }));
    assert_eq!(review.checks.len(), 2);
}

#[test]
fn an_unconfirmed_review_escalates_on_schedule() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-10", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    grant.open_review("erin", 640, &timers).expect("open");
    // Before the deadline nothing happens.
    assert!(grant.escalate_due(&timers, 700).is_none());
    let escalation = grant
        .escalate_due(&timers, 741)
        .expect("the review fell due");
    assert_eq!(escalation.count, 1);
    assert_eq!(
        escalation.to, "dave",
        "it escalates to the request's escalate_to"
    );
    // It re-arms for a second escalation, then stays escalated at the cap.
    let second = grant.escalate_due(&timers, 900).expect("second escalation");
    assert_eq!(second.count, 2);
    assert!(grant.escalate_due(&timers, 1100).is_none());
}

#[test]
fn a_review_escalates_only_when_the_timer_fired() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-11", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    // A port with no registered timer (or one already cleared) does not escalate.
    let timers = InMemoryTimerPort::new();
    grant.open_review("erin", 640, &timers).expect("open");
    timers.clear(
        &spot_bpm_engine::CaseId::new("emergency_eg_11").expect("valid"),
        &spot_dag::NodeId::new("controller_review").expect("valid"),
    );
    assert!(grant.escalate_due(&timers, 900).is_none());
}

#[test]
fn the_grant_round_trips_through_serde() {
    let launcher = FakeLauncher::default();
    let mut grant = active_grant("eg-12", &launcher);
    grant.activate(30).expect("activate");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    grant.open_review("erin", 640, &timers).expect("open");
    let text = serde_json::to_string(&grant).expect("serialisable");
    let back: EmergencyGrant = serde_json::from_str(&text).expect("round trip");
    assert_eq!(back.id(), grant.id());
    assert_eq!(back.review, grant.review);
}

#[test]
fn a_zero_duration_is_refused_at_construction() {
    let err = EmergencyRequest::new(
        "eg-13",
        "ben",
        "Emergency Admin",
        GrantScope::unscoped(),
        "reason",
        "cara",
        "dave",
        0,
    )
    .expect_err("zero is not a duration");
    assert_eq!(err.code(), spot_iam_questions::codes::EMERGENCY_DURATION);
}

#[test]
fn a_blank_reason_is_refused() {
    let err = EmergencyRequest::new(
        "eg-14",
        "ben",
        "Emergency Admin",
        GrantScope::unscoped(),
        "   ",
        "cara",
        "dave",
        60,
    )
    .expect_err("a reason is required");
    assert_eq!(err.code(), spot_iam_questions::codes::REQUEST_INVALID);
}

#[test]
fn a_role_the_policy_does_not_allow_is_refused() {
    let mut request = request("eg-16", 600);
    request.role = spot_iam_roles::RoleName::from("Invoice Poster");
    let err = EmergencyGrant::request(request, policy())
        .expect_err("the policy names only Emergency Admin");
    assert_eq!(err.code(), spot_iam_questions::codes::REQUEST_INVALID);
}

#[test]
fn an_expired_approval_grants_nothing() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-15", 600), policy()).expect("request");
    grant.begin(&launcher, 10).expect("launch");
    grant.expire_approval(20).expect("expire");
    assert!(matches!(grant.phase, EmergencyPhase::Expired { .. }));
    assert!(grant.activate(30).is_err());
}

#[test]
fn only_the_named_approver_or_escalation_target_may_decide() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-9", 600), policy()).expect("request");
    grant.begin(&launcher, 10).expect("launch");
    let err = grant
        .decide_plain(ApprovalDecision::Approve, "mallory", 20)
        .expect_err("a stranger may not approve");
    assert_eq!(err.code(), spot_iam_questions::codes::APPROVAL);
    assert!(grant.activate(21).is_err(), "nothing was granted");
}

#[test]
fn a_late_approval_expires_instead_of_granting() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-10", 600), policy()).expect("request");
    grant.begin(&launcher, 10).expect("launch");
    let late = 10 + i64::try_from(policy().approval_sla_secs).expect("fits") + 1;
    assert!(
        grant
            .decide_plain(ApprovalDecision::Approve, "cara", late)
            .is_err()
    );
    assert!(grant.activate(late + 1).is_err(), "nothing was granted");
}

#[test]
fn the_approver_may_not_be_the_user() {
    let err = EmergencyRequest::new(
        "eg-11",
        "ben",
        "Emergency Admin",
        GrantScope::unscoped(),
        "production incident 4711",
        "ben",
        "dave",
        600,
    )
    .expect_err("self-approval is refused at construction");
    assert_eq!(err.code(), spot_iam_questions::codes::REQUEST_INVALID);
}

// ---------------------------------------------------------------------------------------
// WP-IAM-08.T4: the emergency path decides through the governance crate's approver rule.
// ---------------------------------------------------------------------------------------

const GOV_NOT_APPROVER: &str = "iam_governance.not_the_approver";
const GOV_GRANTEE: &str = "iam_governance.approver_is_grantee";
const GOV_NOT_PENDING: &str = "iam_governance.approval_not_pending";

/// Erin holds `cfo`; nobody else does.
fn erin_is_cfo(actor: &str, role: &str) -> bool {
    actor == "erin" && role == "cfo"
}

fn nobody_holds_roles(_: &str, _: &str) -> bool {
    false
}

/// A launched grant for the grantee `alice`, with the given approver and escalation target.
fn awaiting(id: &str, approver: &str, escalate_to: &str) -> EmergencyGrant {
    let mut grant = EmergencyGrant::request(request(id, 600), policy()).expect("request");
    grant.request.user = "alice".to_owned();
    grant.approval.spec.user = "alice".to_owned();
    grant.request.approver = approver.to_owned();
    grant.approval.spec.approver = approver.to_owned();
    grant.request.escalate_to = EscalationTarget::new(escalate_to).expect("a usable target");
    grant.approval.spec.escalate_to = EscalationTarget::new(escalate_to).expect("a usable target");
    grant.begin(&FakeLauncher::default(), 10).expect("launch");
    grant
}

fn approve() -> ApprovalDecision {
    ApprovalDecision::Approve
}

fn refuse_with(
    grant: &mut EmergencyGrant,
    by: &str,
    at: i64,
    holds: &dyn Fn(&str, &str) -> bool,
    audit: &MemoryEmergencyAuditor,
    code: &str,
) {
    let before = grant.clone();
    let rows = audit.records().len();
    let err = grant
        .decide(approve(), by, at, holds, audit)
        .expect_err("the decision must be refused");
    assert_eq!(err.code(), spot_iam_questions::codes::APPROVAL);
    assert!(
        err.to_string().contains(code),
        "the detail keeps the governance code {code}: {err}"
    );
    assert_eq!(*grant, before, "a refused decision changes nothing");
    let all = audit.records();
    assert_eq!(all.len(), rows + 1, "exactly one audit row per refusal");
    let row = all.last().expect("a row").clone();
    assert_eq!(row.by, by);
    assert_eq!(row.at, at);
    assert_eq!(row.attempted, "approve");
    assert_eq!(row.code, code);
    assert_eq!(row.grant, grant.id());
}

#[test]
fn the_escalation_target_is_refused_before_escalation_and_accepted_after() {
    let audit = MemoryEmergencyAuditor::new();
    let mut grant = awaiting("eg-t1", "ben", "dan");
    refuse_with(
        &mut grant,
        "dan",
        20,
        &nobody_holds_roles,
        &audit,
        GOV_NOT_APPROVER,
    );
    grant.escalate_approval(30).expect("escalate");
    grant
        .decide(approve(), "dan", 40, &nobody_holds_roles, &audit)
        .expect("the escalation target decides after the SLA escalation");
    assert!(grant.approval.is_granted());
    assert_eq!(audit.records().len(), 1, "a success writes no row");
}

#[test]
fn a_role_approver_is_matched_through_the_lookup() {
    let audit = MemoryEmergencyAuditor::new();
    let mut grant = awaiting("eg-t2", "role:cfo", "dan");
    refuse_with(
        &mut grant,
        "mallory",
        20,
        &erin_is_cfo,
        &audit,
        GOV_NOT_APPROVER,
    );
    refuse_with(
        &mut grant,
        "erin",
        21,
        &nobody_holds_roles,
        &audit,
        GOV_NOT_APPROVER,
    );
    grant
        .decide(approve(), "erin", 22, &erin_is_cfo, &audit)
        .expect("the role holder approves");
    assert!(grant.approval.is_granted());
    assert_eq!(audit.records().len(), 2);
}

#[test]
fn the_grantee_is_refused_in_every_state() {
    let audit = MemoryEmergencyAuditor::new();
    let holds_everything = |_: &str, _: &str| true;
    // Before escalation, named as the approver's role holder.
    let mut grant = awaiting("eg-t3", "role:cfo", "dan");
    refuse_with(
        &mut grant,
        "alice",
        20,
        &holds_everything,
        &audit,
        GOV_GRANTEE,
    );
    grant.escalate_approval(30).expect("escalate");
    refuse_with(
        &mut grant,
        "alice",
        31,
        &holds_everything,
        &audit,
        GOV_GRANTEE,
    );
    // Named as the escalation target, before and after.
    let mut grant = awaiting("eg-t3b", "ben", "alice");
    refuse_with(
        &mut grant,
        "alice",
        20,
        &nobody_holds_roles,
        &audit,
        GOV_GRANTEE,
    );
    grant.escalate_approval(30).expect("escalate");
    refuse_with(
        &mut grant,
        "alice",
        31,
        &nobody_holds_roles,
        &audit,
        GOV_GRANTEE,
    );
    assert_eq!(audit.records().len(), 4);
    assert!(!grant.approval.is_granted());
}

#[test]
fn a_stranger_is_refused_before_and_after_escalation() {
    let audit = MemoryEmergencyAuditor::new();
    let mut grant = awaiting("eg-t4", "ben", "dan");
    refuse_with(
        &mut grant,
        "mallory",
        20,
        &nobody_holds_roles,
        &audit,
        GOV_NOT_APPROVER,
    );
    grant.escalate_approval(30).expect("escalate");
    refuse_with(
        &mut grant,
        "mallory",
        31,
        &nobody_holds_roles,
        &audit,
        GOV_NOT_APPROVER,
    );
}

#[test]
fn a_rejection_by_a_stranger_is_audited_as_a_rejection() {
    let audit = MemoryEmergencyAuditor::new();
    let mut grant = awaiting("eg-t4b", "ben", "dan");
    let before = grant.clone();
    grant
        .decide(
            ApprovalDecision::Reject {
                reason: "secret reason typed by mallory".to_owned(),
            },
            "mallory",
            20,
            &nobody_holds_roles,
            &audit,
        )
        .expect_err("refused");
    assert_eq!(grant, before);
    let row = audit.last().expect("a row");
    assert_eq!(row.attempted, "reject");
    let json = serde_json::to_string(&row).expect("serialises");
    assert!(
        !json.contains("secret reason"),
        "no text typed by the refused person reaches the audit row"
    );
}

#[test]
fn a_decision_on_a_decided_or_lapsed_approval_is_refused_and_audited() {
    let audit = MemoryEmergencyAuditor::new();
    // Already decided: the phase is still awaiting activation, the approval is not pending.
    let mut grant = awaiting("eg-t5", "ben", "dan");
    grant
        .decide(approve(), "ben", 20, &nobody_holds_roles, &audit)
        .expect("approve");
    refuse_with(
        &mut grant,
        "ben",
        21,
        &nobody_holds_roles,
        &audit,
        GOV_NOT_PENDING,
    );

    // Lapsed: the lapse still expires the approval and the phase.
    let mut grant = awaiting("eg-t5b", "ben", "dan");
    let late = 10 + i64::try_from(policy().approval_sla_secs).expect("fits") + 1;
    let rows = audit.records().len();
    let err = grant
        .decide(approve(), "ben", late, &nobody_holds_roles, &audit)
        .expect_err("lapsed");
    assert_eq!(err.code(), spot_iam_questions::codes::APPROVAL);
    assert!(matches!(grant.phase, EmergencyPhase::Expired { .. }));
    assert_eq!(audit.records().len(), rows + 1);
    assert_eq!(audit.last().expect("row").code, GOV_NOT_PENDING);
    assert!(grant.activate(late + 1).is_err());
}

#[test]
fn a_decision_in_the_wrong_phase_is_audited_with_the_questions_code() {
    let audit = MemoryEmergencyAuditor::new();
    let mut grant = EmergencyGrant::request(request("eg-t5c", 600), policy()).expect("request");
    let before = grant.clone();
    let err = grant
        .decide(approve(), "cara", 20, &nobody_holds_roles, &audit)
        .expect_err("not awaiting approval");
    assert_eq!(err.code(), spot_iam_questions::codes::EMERGENCY_STATE);
    assert_eq!(grant, before);
    assert_eq!(audit.records().len(), 1);
    assert_eq!(
        audit.last().expect("row").code,
        spot_iam_questions::codes::EMERGENCY_STATE
    );
}

#[test]
fn escalate_approval_is_idempotent_and_needs_a_pending_approval() {
    let mut grant = awaiting("eg-t6", "ben", "dan");
    grant.escalate_approval(30).expect("escalate");
    let once = grant.clone();
    grant
        .escalate_approval(99)
        .expect("a second call is not an error");
    assert_eq!(grant, once, "a second call changes nothing");
    assert!(grant.approval.escalated);
    assert!(
        matches!(grant.phase, EmergencyPhase::AwaitingApproval { .. }),
        "escalating decides nothing"
    );

    let audit = MemoryEmergencyAuditor::new();
    grant
        .decide(approve(), "ben", 40, &nobody_holds_roles, &audit)
        .expect("approve");
    // Approved, so no longer pending: the approval refuses it.
    assert!(grant.escalate_approval(50).is_err());
    // Not awaiting approval at all.
    let mut fresh = EmergencyGrant::request(request("eg-t6b", 600), policy()).expect("request");
    let err = fresh.escalate_approval(5).expect_err("not launched");
    assert_eq!(err.code(), spot_iam_questions::codes::EMERGENCY_STATE);
    let mut expired = awaiting("eg-t6c", "ben", "dan");
    expired.expire_approval(20).expect("expire");
    assert!(expired.escalate_approval(30).is_err());
}

#[test]
fn a_success_writes_no_audit_row_and_each_refusal_exactly_one() {
    let audit = MemoryEmergencyAuditor::new();
    let mut grant = awaiting("eg-t7", "ben", "dan");
    for (i, who) in ["mallory", "dan", "alice"].into_iter().enumerate() {
        let at = 20 + i64::try_from(i).expect("small");
        let _ = grant
            .decide(approve(), who, at, &nobody_holds_roles, &audit)
            .expect_err("refused");
        assert_eq!(audit.records().len(), i + 1);
    }
    grant
        .decide(approve(), "ben", 30, &nobody_holds_roles, &audit)
        .expect("approve");
    assert_eq!(audit.records().len(), 3);
}

#[test]
fn an_escalated_grant_round_trips_and_an_old_snapshot_still_loads() {
    let mut grant = awaiting("eg-t9", "ben", "dan");
    grant.escalate_approval(30).expect("escalate");
    let json = serde_json::to_value(&grant).expect("serialises");
    let back: EmergencyGrant = serde_json::from_value(json.clone()).expect("round trip");
    assert_eq!(back, grant);
    assert!(back.approval.escalated);

    let mut old = json;
    let approval = old
        .get_mut("approval")
        .and_then(serde_json::Value::as_object_mut)
        .expect("approval object");
    approval.remove("escalated");
    approval.remove("escalated_at");
    let loaded: EmergencyGrant = serde_json::from_value(old).expect("old snapshot loads");
    assert!(!loaded.approval.escalated);
}

// ---------------------------------------------------------------------------------------
// WP-IAM-08.T6: an emergency request cannot hold an unusable escalation target.
// ---------------------------------------------------------------------------------------

fn bad_targets() -> Vec<String> {
    vec![
        String::new(),
        "   ".to_owned(),
        "dave\u{0}".to_owned(),
        "da\nve".to_owned(),
        "x".repeat(spot_iam_governance::MAX_TARGET_BYTES + 1),
    ]
}

/// Whether `text` echoes the offered value: a control character it carried, or a run of the
/// over-long value. The blank cases have nothing to echo.
fn echoes(text: &str, offered: &str) -> bool {
    if offered.chars().any(char::is_control) {
        return text.chars().any(char::is_control);
    }
    if offered.len() > spot_iam_governance::MAX_TARGET_BYTES {
        return text.contains(&"x".repeat(8));
    }
    false
}

#[test]
fn an_unusable_escalation_target_cannot_be_built() {
    for bad in bad_targets() {
        let err = EmergencyRequest::new(
            "eg-t6-1",
            "ben",
            "Emergency Admin",
            GrantScope::unscoped(),
            "production incident 4711",
            "cara",
            &bad,
            600,
        )
        .expect_err("an unusable target is refused");
        assert_eq!(
            err.code(),
            spot_iam_questions::codes::ESCALATION_TARGET_INVALID,
            "{bad:?}"
        );
        assert!(
            err.to_string().starts_with("eg-t6-1: "),
            "the refusal names the request: {err}"
        );
        let text = err.to_string();
        assert!(!echoes(&text, &bad), "the offered value is echoed: {text}");
    }
    // The whole emergency grant is never created for a refused request.
    assert!(
        EmergencyRequest::new(
            "eg-t6-2",
            "ben",
            "Emergency Admin",
            GrantScope::unscoped(),
            "production incident 4711",
            "cara",
            "",
            600,
        )
        .is_err(),
        "no request exists without a target"
    );
}

#[test]
fn a_refused_request_is_audited_without_the_offered_target() {
    let audit = MemoryEmergencyAuditor::new();
    let err = EmergencyRequest::open(
        "eg-t6-3",
        "ben",
        "Emergency Admin",
        GrantScope::unscoped(),
        "production incident 4711",
        "cara",
        "dave\u{0}",
        600,
        42,
        &audit,
    )
    .expect_err("refused");
    assert_eq!(
        err.code(),
        spot_iam_questions::codes::ESCALATION_TARGET_INVALID
    );
    let rows = audit.records();
    assert_eq!(rows.len(), 1, "exactly one audit row per refusal");
    let row = rows.first().expect("a row");
    assert_eq!(row.grant, "eg-t6-3");
    assert_eq!(row.by, "ben");
    assert_eq!(row.attempted, "request");
    assert_eq!(row.at, 42);
    assert_eq!(
        row.code,
        spot_iam_questions::codes::ESCALATION_TARGET_INVALID
    );
    let json = serde_json::to_string(row).expect("serialises");
    assert!(
        !json.contains("dave\\u0000") && !json.contains("dave\u{0}"),
        "the offered target never reaches the audit row: {json}"
    );
}

#[test]
fn an_accepted_request_writes_no_audit_row() {
    let audit = MemoryEmergencyAuditor::new();
    let request = EmergencyRequest::open(
        "eg-t6-4",
        "ben",
        "Emergency Admin",
        GrantScope::unscoped(),
        "production incident 4711",
        "cara",
        "dave",
        600,
        42,
        &audit,
    )
    .expect("a usable request");
    assert_eq!(request.escalate_to.as_str(), "dave");
    assert!(audit.records().is_empty(), "a success writes no row");
}

#[test]
fn a_stored_request_with_an_unusable_target_fails_the_load() {
    let mut json = serde_json::to_value(request("eg-t6-5", 600)).expect("json");
    json["escalate_to"] = serde_json::Value::String(String::new());
    let err = serde_json::from_value::<EmergencyRequest>(json).expect_err("refused");
    assert!(
        !err.to_string().contains("escalate_to\": \"\""),
        "the value is not echoed"
    );
}

#[test]
fn a_valid_request_carries_its_target_through_the_whole_flow() {
    let launcher = FakeLauncher::default();
    let mut grant = EmergencyGrant::request(request("eg-t6-6", 600), policy()).expect("request");
    assert_eq!(grant.request.escalate_to.as_str(), "dave");
    assert_eq!(grant.approval.spec.escalate_to.as_str(), "dave");
    grant.begin(&launcher, 10).expect("launch");
    grant
        .decide_plain(ApprovalDecision::Approve, "cara", 20)
        .expect("the approver approves");
    let assignment = grant.activate(30).expect("activate").clone();
    assert_eq!(assignment.source, Source::Emergency);
    assert_eq!(assignment.id, "emergency:eg-t6-6");
    grant.end(630).expect("end");
    let timers = InMemoryTimerPort::new();
    let review = grant
        .open_review("erin", 640, &timers)
        .expect("open")
        .clone();
    assert_eq!(
        review.escalate_to, "dave",
        "the review escalates to the request's validated target"
    );
    let escalation = grant
        .escalate_due(&timers, 741)
        .expect("the review fell due");
    assert_eq!(escalation.to, "dave");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn decide_matches_the_brute_force_oracle(
        actor in prop::sample::select(vec!["alice", "ben", "dan", "erin", "mallory"]),
        approver in prop::sample::select(vec!["ben", "role:cfo"]),
        escalate_to in prop::sample::select(vec!["dan", "role:ops", "alice"]),
        escalated in any::<bool>(),
        holds_cfo in any::<bool>(),
        holds_ops in any::<bool>(),
    ) {
        let holds = move |_: &str, role: &str| match role {
            "cfo" => holds_cfo,
            "ops" => holds_ops,
            _ => false,
        };
        let matches = |target: &str| match target.strip_prefix("role:") {
            Some(role) => holds(actor, role),
            None => actor == target,
        };
        let expected = actor != "alice" && (matches(approver) || (escalated && matches(escalate_to)));

        let audit = MemoryEmergencyAuditor::new();
        let mut grant = awaiting("eg-prop", approver, escalate_to);
        if escalated {
            grant.escalate_approval(30).expect("escalate");
        }
        let before = grant.clone();
        let result = grant.decide(approve(), actor, 40, &holds, &audit);
        prop_assert_eq!(result.is_ok(), expected);
        if expected {
            prop_assert!(grant.approval.is_granted());
            prop_assert!(audit.records().is_empty());
        } else {
            prop_assert_eq!(&grant, &before);
            prop_assert_eq!(audit.records().len(), 1);
        }
    }
}
