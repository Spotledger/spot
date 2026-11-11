//! Grant approval on the BPM engine: idempotent launch, the approver-not-the-grantee rule,
//! and the guarantee that nothing is granted without approval.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::cell::{Cell, RefCell};

use common::request_id;
use spot_bpm_model::ProcessVersion;
use spot_iam_governance::{
    ApprovalDecision, ApprovalSpec, ApprovalState, EscalationTarget, GovError, GrantApproval,
    GrantScope, LaunchReceipt, ProcessLauncher, approval_process,
};
use spot_iam_roles::{RoleName, Source};

/// A launcher that records how often it was called and the process it was given.
#[derive(Default)]
struct FakeLauncher {
    launches: Cell<usize>,
    hash: RefCell<Option<String>>,
}

impl ProcessLauncher for FakeLauncher {
    fn launch(
        &self,
        process: &ProcessVersion,
        request: &spot_iam_governance::GrantRequestId,
    ) -> Result<LaunchReceipt, GovError> {
        self.launches.set(self.launches.get() + 1);
        *self.hash.borrow_mut() = Some(process.content_hash().as_hex().to_owned());
        Ok(LaunchReceipt {
            case_id: format!("case:{}", request.as_str()),
            process_hash: process.content_hash().as_hex().to_owned(),
        })
    }
}

fn spec() -> ApprovalSpec {
    ApprovalSpec {
        request: request_id("req-1"),
        user: "ben".to_owned(),
        role: RoleName::from("Finance Approver"),
        scope: GrantScope::value("Company", "Company A"),
        reason: "the role needs approval".to_owned(),
        approver: "role:cfo".to_owned(),
        escalate_to: EscalationTarget::new("role:ceo").expect("target"),
        sla_secs: 3600,
    }
}

/// Only `cfo` and `ben` hold the `cfo` role; `ben` is the grantee.
fn cfo_holder(actor: &str, role: &str) -> bool {
    role == "cfo" && (actor == "cfo" || actor == "ben")
}

fn pending(approver: &str) -> GrantApproval {
    let mut spec = spec();
    spec.approver = approver.to_owned();
    let mut approval = GrantApproval::new(spec);
    approval.begin(&FakeLauncher::default(), 0).expect("launch");
    approval
}

#[test]
fn begin_launches_the_engine_at_most_once() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    let first = approval.begin(&launcher, 10).expect("launch").clone();
    let second = approval.begin(&launcher, 20).expect("relaunch").clone();
    assert_eq!(launcher.launches.get(), 1, "the approval launched twice");
    assert_eq!(first, second);
    assert_eq!(first.case_id, "case:req-1");
    assert_eq!(approval.state, ApprovalState::Pending { launched_at: 10 });
    assert_eq!(approval.expires_at(), Some(3610));
}

#[test]
fn the_launched_process_is_the_built_in_template_with_the_sla() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    let expected = approval_process(&spec()).expect("process");
    assert_eq!(
        launcher.hash.borrow().as_deref(),
        Some(expected.content_hash().as_hex())
    );
}

#[test]
fn the_grant_appears_only_after_approval() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    let err = approval.assignment().expect_err("nothing granted yet");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_APPROVED);

    approval
        .decide_with_roles(ApprovalDecision::Approve, "cfo", 30, &cfo_holder)
        .expect("approve");
    assert!(approval.is_granted());
    let assignment = approval.assignment().expect("granted");
    assert_eq!(assignment.id, "grant:req-1");
    assert_eq!(assignment.user, "ben");
    assert_eq!(assignment.role, RoleName::from("Finance Approver"));
    assert_eq!(assignment.source, Source::Manual);
    assert_eq!(
        spot_iam_governance::GrantScope::from_scope(&assignment.scope),
        GrantScope::value("Company", "Company A")
    );
}

#[test]
fn a_rejected_approval_grants_nothing() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    approval
        .decide_with_roles(
            ApprovalDecision::Reject {
                reason: "not this quarter".to_owned(),
            },
            "cfo",
            30,
            &cfo_holder,
        )
        .expect("reject");
    assert!(!approval.is_granted());
    assert_eq!(
        approval.assignment().expect_err("nothing").code(),
        spot_iam_governance::codes::NOT_APPROVED
    );
}

#[test]
fn an_expired_approval_grants_nothing() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    approval.expire(3600).expect("expire");
    approval.expire(3601).expect("expire is idempotent");
    assert_eq!(approval.state, ApprovalState::Expired { at: 3600 });
    assert_eq!(
        approval.assignment().expect_err("nothing").code(),
        spot_iam_governance::codes::NOT_APPROVED
    );
}

#[test]
fn the_approver_cannot_be_the_grantee() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "ben", 30, &|_, _| false)
        .expect_err("self-approval");
    assert_eq!(err.code(), spot_iam_governance::codes::APPROVER_IS_GRANTEE);
    assert!(
        approval.state.is_pending(),
        "a refused approval stayed pending"
    );
}

#[test]
fn a_decision_on_a_decided_approval_is_refused() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    approval
        .decide_with_roles(ApprovalDecision::Approve, "cfo", 30, &cfo_holder)
        .expect("approve");
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "cfo", 40, &cfo_holder)
        .expect_err("already decided");
    assert_eq!(err.code(), spot_iam_governance::codes::APPROVAL_NOT_PENDING);
}

#[test]
fn a_crash_between_approval_and_grant_is_retried_idempotently() {
    let launcher = FakeLauncher::default();
    let mut approval = GrantApproval::new(spec());
    approval.begin(&launcher, 0).expect("launch");
    approval
        .decide_with_roles(ApprovalDecision::Approve, "cfo", 30, &cfo_holder)
        .expect("approve");
    // The host crashes after approval and retries the application step: the assignment id is
    // derived from the request id, so the retry writes the same assignment.
    let first = approval.assignment().expect("first");
    let second = approval.assignment().expect("retry");
    assert_eq!(first, second);
}

#[test]
fn the_named_person_approves() {
    let mut approval = pending("cara");
    approval
        .decide_with_roles(ApprovalDecision::Approve, "cara", 30, &|_, _| false)
        .expect("the named approver");
    assert!(approval.is_granted());
}

#[test]
fn someone_else_is_refused_and_nothing_changes() {
    let mut approval = pending("cara");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "mallory", 30, &|_, _| false)
        .expect_err("not the approver");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    assert_eq!(
        err.subject(),
        "req-1",
        "the refusal names the request for the audit record"
    );
    assert_eq!(approval, before, "a refused decision changes nothing");
    let err = approval
        .decide_with_roles(
            ApprovalDecision::Reject {
                reason: "x".to_owned(),
            },
            "mallory",
            30,
            &|_, _| false,
        )
        .expect_err("a rejection is refused too");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    assert_eq!(approval, before);
}

#[test]
fn the_escalation_target_cannot_decide_through_the_approver_check() {
    let mut approval = pending("cara");
    approval.spec.escalate_to = EscalationTarget::new("dan").expect("target");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "dan", 30, &|_, _| false)
        .expect_err("escalate_to is not the approver");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    assert_eq!(approval, before);
}

#[test]
fn a_role_approver_is_resolved_by_the_caller() {
    let mut approval = pending("role:cfo");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "mallory", 30, &cfo_holder)
        .expect_err("without the role");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    assert_eq!(approval, before);
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "cfo", 30, &|_, _| false)
        .expect_err("no role lookup, no role approver");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    approval
        .decide_with_roles(ApprovalDecision::Approve, "cfo", 30, &cfo_holder)
        .expect("with the role");
    assert!(approval.is_granted());
}

#[test]
fn a_grantee_holding_the_approver_role_is_still_refused() {
    let mut approval = pending("role:cfo");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "ben", 30, &cfo_holder)
        .expect_err("the grantee holds the role");
    assert_eq!(err.code(), spot_iam_governance::codes::APPROVER_IS_GRANTEE);
    assert_eq!(approval, before);
}

#[test]
fn an_expired_approval_stays_expired_when_the_approver_decides() {
    let mut approval = pending("cara");
    approval.expire(3600).expect("expire");
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "cara", 3700, &|_, _| false)
        .expect_err("expired");
    assert_eq!(err.code(), spot_iam_governance::codes::APPROVAL_NOT_PENDING);
    assert_eq!(approval.state, ApprovalState::Expired { at: 3600 });
}

fn escalating(approver: &str, escalate_to: &str) -> GrantApproval {
    let mut approval = pending(approver);
    approval.spec.escalate_to = EscalationTarget::new(escalate_to).expect("target");
    approval
}

#[test]
fn the_escalation_target_is_refused_before_escalation_and_accepted_after() {
    let mut approval = escalating("cara", "dan");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "dan", 30, &|_, _| false)
        .expect_err("not escalated yet");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    assert_eq!(approval, before, "a refusal must not change the approval");

    approval.escalate(3700).expect("escalate");
    assert!(approval.escalated);
    approval
        .decide_with_roles(ApprovalDecision::Approve, "dan", 3800, &|_, _| false)
        .expect("the escalation target decides after escalation");
    assert!(approval.is_granted());
}

#[test]
fn a_role_escalation_target_is_resolved_by_the_caller_after_escalation() {
    let mut approval = escalating("cara", "role:ceo");
    let ceo = |actor: &str, role: &str| role == "ceo" && actor == "erin";
    approval
        .decide_with_roles(ApprovalDecision::Approve, "erin", 30, &ceo)
        .expect_err("not escalated yet");
    approval.escalate(3700).expect("escalate");
    approval
        .decide_with_roles(ApprovalDecision::Approve, "erin", 3800, &ceo)
        .expect("ceo decides after escalation");
}

#[test]
fn the_original_approver_may_still_decide_after_escalation() {
    let mut approval = escalating("cara", "dan");
    approval.escalate(3700).expect("escalate");
    approval
        .decide_with_roles(ApprovalDecision::Approve, "cara", 3800, &|_, _| false)
        .expect("the named approver still decides");
}

#[test]
fn the_grantee_is_refused_even_after_escalation() {
    let mut approval = escalating("cara", "ben");
    approval.escalate(3700).expect("escalate");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "ben", 3800, &|_, _| true)
        .expect_err("the grantee never approves");
    assert_eq!(err.code(), spot_iam_governance::codes::APPROVER_IS_GRANTEE);
    assert_eq!(approval, before);
}

#[test]
fn a_stranger_is_refused_after_escalation_and_state_is_unchanged() {
    let mut approval = escalating("cara", "dan");
    approval.escalate(3700).expect("escalate");
    let before = approval.clone();
    let err = approval
        .decide_with_roles(ApprovalDecision::Approve, "mallory", 3800, &|_, _| false)
        .expect_err("not an approver");
    assert_eq!(err.code(), spot_iam_governance::codes::NOT_THE_APPROVER);
    assert_eq!(approval, before);
}

#[test]
fn plain_decide_accepts_a_named_escalation_target_after_escalation() {
    let mut approval = escalating("cara", "dan");
    approval.escalate(3700).expect("escalate");
    approval
        .decide_with_roles(ApprovalDecision::Approve, "dan", 3800, &|_, _| false)
        .expect("a named escalation target is a named user");
}

#[test]
fn escalating_a_decided_approval_is_refused_and_escalating_twice_is_idempotent() {
    let mut approval = escalating("cara", "dan");
    approval.escalate(3700).expect("escalate");
    approval.escalate(9999).expect("again");
    assert_eq!(
        approval.escalated_at,
        Some(3700),
        "the first escalation time stays"
    );
    approval
        .decide_with_roles(ApprovalDecision::Approve, "cara", 4000, &|_, _| false)
        .expect("decide");
    let err = approval.escalate(5000).expect_err("already decided");
    assert_eq!(err.code(), spot_iam_governance::codes::APPROVAL_NOT_PENDING);
}

#[test]
fn an_old_record_without_the_flag_loads_unescalated() {
    let approval = escalating("cara", "dan");
    let mut value = serde_json::to_value(&approval).expect("to json");
    let object = value.as_object_mut().expect("object");
    object.remove("escalated");
    object.remove("escalated_at");
    let loaded: GrantApproval = serde_json::from_value(value).expect("old record loads");
    assert!(!loaded.escalated);
    assert_eq!(loaded.escalated_at, None);
    assert_eq!(loaded, approval);
}
