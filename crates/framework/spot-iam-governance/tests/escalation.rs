//! Recorded security finding: an escalation target is a type nothing unusable can hold — refused at build, at
//! load, at the gate, and never matching an actor.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_iam_governance::{
    ApprovalDecision, ApprovalPolicy, ApprovalSpec, EscalationTarget, GrantApproval,
    GrantRequestId, GrantScope, MAX_TARGET_BYTES, approval_process, codes,
};
use spot_iam_roles::RoleName;

fn spec(escalate_to: &str) -> ApprovalSpec {
    ApprovalSpec {
        request: GrantRequestId::new("req-1").expect("id"),
        user: "ben".to_owned(),
        role: RoleName::from("Finance Approver"),
        scope: GrantScope::value("Company", "A"),
        reason: "needs approval".to_owned(),
        approver: "cara".to_owned(),
        escalate_to: EscalationTarget::new(escalate_to).expect("a usable target"),
        sla_secs: 3600,
    }
}

fn bad_targets() -> Vec<String> {
    vec![
        String::new(),
        "   ".to_owned(),
        "a\0b".to_owned(),
        "a\nb".to_owned(),
        "role:".to_owned(),
        "x".repeat(MAX_TARGET_BYTES + 1),
    ]
}

#[test]
fn the_constructor_refuses_bad_targets() {
    for bad in bad_targets() {
        let err = EscalationTarget::new(&bad).expect_err("refused");
        assert_eq!(err.code(), codes::REQUEST_INVALID);
    }
    assert!(EscalationTarget::new("role:ceo").is_ok());
}

#[test]
fn the_policy_builder_refuses_bad_targets() {
    for bad in bad_targets() {
        let err = ApprovalPolicy::none()
            .try_routed_to("role:cfo", &bad, 60)
            .expect_err("refused");
        assert_eq!(err.code(), codes::REQUEST_INVALID);
        // The infallible builder fails closed: no target is stored.
        let policy = ApprovalPolicy::none().routed_to("role:cfo", &bad, 60);
        assert_eq!(policy.escalate_to, None);
    }
}

#[test]
fn a_stored_policy_with_a_bad_target_fails_the_whole_load() {
    for bad in bad_targets() {
        let json = serde_json::json!({
            "roles": ["Finance Approver"],
            "approver": "role:cfo",
            "escalate_to": bad,
            "sla_secs": 60
        });
        let err = serde_json::from_value::<ApprovalPolicy>(json).expect_err("refused");
        let text = err.to_string();
        assert!(text.contains(codes::REQUEST_INVALID), "{text}");
        assert!(
            !text.contains("a\\u0000b"),
            "the value must not be echoed: {text}"
        );
    }
    let ok = serde_json::json!({"approver": "role:cfo", "escalate_to": "role:ceo"});
    let policy: ApprovalPolicy = serde_json::from_value(ok).expect("valid");
    assert_eq!(policy.escalate_to.expect("target").as_str(), "role:ceo");
}

#[test]
fn a_stored_spec_with_a_bad_target_fails_the_load_naming_the_request_not_the_value() {
    for bad in bad_targets() {
        let mut json = serde_json::to_value(spec("dan")).expect("json");
        json["escalate_to"] = serde_json::Value::String(bad);
        let err = serde_json::from_value::<ApprovalSpec>(json).expect_err("refused");
        let text = err.to_string();
        assert!(text.contains(codes::REQUEST_INVALID), "{text}");
        assert!(text.contains("req-1"), "{text}");
    }
}

#[test]
fn a_stored_approval_with_a_bad_target_fails_the_load() {
    let approval = GrantApproval::new(spec("dan"));
    let mut json = serde_json::to_value(&approval).expect("json");
    json["spec"]["escalate_to"] = serde_json::Value::String(String::new());
    serde_json::from_value::<GrantApproval>(json).expect_err("refused");
    let ok = serde_json::to_value(&approval).expect("json");
    serde_json::from_value::<GrantApproval>(ok).expect("valid");
}

#[test]
fn a_spec_cannot_hold_a_target_that_builds_no_process() {
    // The hand-built path used to reach `approval_process` with a bad target and be refused
    // there. The type closes that path: every bad value is refused before a spec exists, so
    // the process builder only ever sees a usable target.
    for bad in bad_targets() {
        assert!(
            EscalationTarget::new(&bad).is_err(),
            "no spec can carry {bad:?}"
        );
    }
    let process = approval_process(&spec("dan")).expect("a usable target builds a process");
    let escalation = process
        .node(&spot_bpm_model::NodeId::new("escalate").expect("id"))
        .expect("escalate node");
    match &escalation.kind {
        spot_bpm_model::NodeKind::Decision(config) => {
            assert_eq!(config.assignee.as_deref(), Some("dan"));
        }
        other => panic!("escalate is not a decision: {other:?}"),
    }
}

#[test]
fn an_empty_actor_matches_no_escalation_target_even_after_escalation() {
    // The target is always usable now; the actor id is what a request can still get wrong.
    for target in ["dan", "role:ceo"] {
        let mut approval = GrantApproval::new(spec(target));
        approval.state = spot_iam_governance::approval::ApprovalState::Pending { launched_at: 0 };
        approval.escalated = true;
        let before = approval.clone();
        for actor in ["", "  ", "\0"] {
            let err = approval
                .decide_with_roles(ApprovalDecision::Approve, actor, 30, &|_, _| true)
                .expect_err("nobody in particular is not an approver");
            assert_eq!(err.code(), codes::NOT_THE_APPROVER);
            assert_eq!(approval, before);
        }
    }
}

#[test]
fn an_empty_approver_matches_an_empty_actor_no_more_than_an_empty_escalation_target() {
    let mut approval = GrantApproval::new(spec("dan"));
    approval.spec.approver = String::new();
    approval.state = spot_iam_governance::approval::ApprovalState::Pending { launched_at: 0 };
    approval
        .decide_with_roles(ApprovalDecision::Approve, "", 30, &|_, _| true)
        .expect_err("empty actor");
}

#[test]
fn one_bad_rule_in_a_stored_table_loads_none_of_the_rules() {
    let table = serde_json::json!([
        {"approver": "role:cfo", "escalate_to": "role:ceo"},
        {"approver": "role:cfo", "escalate_to": ""}
    ]);
    serde_json::from_value::<Vec<ApprovalPolicy>>(table).expect_err("all or nothing");
}
