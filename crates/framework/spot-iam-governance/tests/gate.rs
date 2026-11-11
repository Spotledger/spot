//! The grant gate: blocking, overriding, delegated administration and the IdP-managed refusal.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assignment, catalogue, hierarchy, request, request_id, rules};
use spot_iam_assign::ManagedRoles;
use spot_iam_governance::{
    ApprovalPolicy, Conflict, DelegationPolicy, GovernanceState, GrantDecision, GrantGate,
    GrantScope, Grantor, MitigatingControl, OverrideSubmission, ScopeHierarchy, SodOverrideRecord,
};
use spot_iam_roles::{RoleName, Source};

const A: &str = "Company A";
const B: &str = "Company B";

fn scope(value: &str) -> GrantScope {
    GrantScope::value("Company", value)
}

fn base() -> GovernanceState {
    GovernanceState::without_portal_users()
        .with_rules(rules())
        .with_catalogue(catalogue())
        .with_hierarchy(ScopeHierarchy::new())
        .with_delegation(DelegationPolicy::Unrestricted)
}

fn conflict_state() -> GovernanceState {
    base().with_assignments(vec![assignment("jane", "Finance Approver", Some(scope(A)))])
}

fn override_submission() -> OverrideSubmission {
    OverrideSubmission::new(
        "month-end close only",
        MitigatingControl::new("C-1", "second reviewer"),
    )
}

#[test]
fn a_conflicting_grant_is_blocked_when_the_grantor_lacks_override_sod() {
    let gate = GrantGate::new();
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::plain("admin"),
    );
    let decision = gate
        .evaluate(&req, &conflict_state())
        .expect("the gate decides");
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("expected Blocked, got {decision:#?}");
    };
    assert_eq!(conflicts.len(), 2);
    assert!(conflicts.iter().all(Conflict::is_overridable));
}

#[test]
fn a_conflicting_grant_needs_an_override_when_the_grantor_may_override() {
    let gate = GrantGate::new();
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::new("admin", ["override_sod"]),
    );
    let decision = gate
        .evaluate(&req, &conflict_state())
        .expect("the gate decides");
    let GrantDecision::NeedsOverride(conflicts) = decision else {
        panic!("expected NeedsOverride, got {decision:#?}");
    };
    assert_eq!(conflicts.len(), 2);
}

#[test]
fn a_grantor_without_the_right_cannot_force_a_conflict_with_an_override() {
    let gate = GrantGate::new();
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::plain("admin"),
    )
    .with_override(override_submission());
    let decision = gate
        .evaluate(&req, &conflict_state())
        .expect("the gate decides");
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("expected Blocked, got {decision:#?}");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0].code(),
        spot_iam_governance::codes::OVERRIDE_NOT_PERMITTED
    );
}

#[test]
fn an_incomplete_override_is_blocked() {
    let gate = GrantGate::new();
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::new("admin", ["override_sod"]),
    )
    .with_override(OverrideSubmission::new(
        "  ",
        MitigatingControl::new("C-1", "review"),
    ));
    let decision = gate
        .evaluate(&req, &conflict_state())
        .expect("the gate decides");
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("expected Blocked, got {decision:#?}");
    };
    assert_eq!(
        conflicts[0].code(),
        spot_iam_governance::codes::OVERRIDE_INCOMPLETE
    );
}

#[test]
fn a_complete_override_allows_and_records_the_auditable_records() {
    let gate = GrantGate::new();
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::new("admin", ["override_sod"]),
    )
    .with_override(override_submission());
    let state = conflict_state();
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    assert!(decision.is_allowed(), "{decision:#?}");

    let conflicts = gate.sod_conflicts(&req, &state);
    let records = gate
        .record_overrides(&req, &conflicts, 100)
        .expect("records");
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(SodOverrideRecord::is_complete));
    assert!(
        records
            .iter()
            .all(|r| r.granted_by == "admin" && r.granted_at == 100)
    );
    // The override is the note an access review shows.
    assert!(records[0].access_review_note().contains("second reviewer"));
}

#[test]
fn an_existing_override_allows_a_later_grant_without_a_new_submission() {
    let gate = GrantGate::new();
    let overrides = vec![
        SodOverrideRecord {
            rule_id: "sod.vendor-master.payments".to_owned(),
            user: "jane".to_owned(),
            scope: Some(A.to_owned()),
            reason: "reviewed".to_owned(),
            mitigating_control: MitigatingControl::new("C-1", "review"),
            granted_by: "admin".to_owned(),
            granted_at: 1,
        },
        SodOverrideRecord {
            rule_id: "sod.purchase-order.approval".to_owned(),
            user: "jane".to_owned(),
            scope: Some(A.to_owned()),
            reason: "reviewed".to_owned(),
            mitigating_control: MitigatingControl::new("C-1", "review"),
            granted_by: "admin".to_owned(),
            granted_at: 1,
        },
    ];
    let state = conflict_state().with_overrides(overrides);
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::new("admin", ["override_sod"]),
    );
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    assert!(decision.is_allowed(), "{decision:#?}");
}

#[test]
fn delegation_refuses_a_role_the_grantor_does_not_hold() {
    let gate = GrantGate::new();
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::plain("admin"),
    );
    let decision = gate
        .evaluate(&req, &base().with_delegation(DelegationPolicy::WithinHeld))
        .expect("the gate decides");
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("expected Blocked, got {decision:#?}");
    };
    assert_eq!(
        conflicts[0].code(),
        spot_iam_governance::codes::DELEGATION_DENIED
    );
}

#[test]
fn delegation_refuses_a_scope_the_grantor_does_not_hold() {
    let gate = GrantGate::new();
    let state = base()
        .with_delegation(DelegationPolicy::WithinHeld)
        .with_assignments(vec![assignment(
            "ana",
            "Accounts Payable Clerk",
            Some(scope(A)),
        )]);
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(B)),
        Grantor::plain("ana"),
    );
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("expected Blocked, got {decision:#?}");
    };
    assert_eq!(
        conflicts[0].code(),
        spot_iam_governance::codes::DELEGATION_DENIED
    );
}

#[test]
fn delegation_allows_a_scope_the_grantor_holds_with_descendants() {
    let gate = GrantGate::new();
    let mut ana = assignment("ana", "Accounts Payable Clerk", Some(scope(A)));
    ana.scope = scope(A).with_descendants().to_scope();
    let state = base()
        .with_assignments(vec![ana])
        .with_hierarchy(hierarchy())
        .with_delegation(DelegationPolicy::WithinHeld);
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope("Company A/Division 1")),
        Grantor::plain("ana"),
    );
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    assert!(decision.is_allowed(), "{decision:#?}");
}

#[test]
fn an_idp_managed_role_is_refused_even_when_the_grantor_holds_it() {
    let gate = GrantGate::new();
    let mut managed = ManagedRoles::new();
    managed.declare(RoleName::from("Finance Approver"), "okta");
    let state = base()
        .with_assignments(vec![assignment(
            "admin",
            "Finance Approver",
            Some(scope(A)),
        )])
        .with_managed_roles(managed);
    let req = request(
        "r1",
        "jane",
        "Finance Approver",
        Some(scope(A)),
        Grantor::plain("admin"),
    );
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("expected Blocked, got {decision:#?}");
    };
    assert_eq!(conflicts[0].code(), spot_iam_governance::codes::IDP_MANAGED);
}

#[test]
fn delegation_is_unrestricted_when_the_policy_says_so() {
    let gate = GrantGate::new();
    let state = base().with_delegation(DelegationPolicy::Unrestricted);
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::plain("admin"),
    );
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    assert!(decision.is_allowed(), "{decision:#?}");
}

#[test]
fn the_approval_policy_is_reported_after_the_sod_check() {
    let gate = GrantGate::new();
    let state = conflict_state()
        .with_delegation(DelegationPolicy::Unrestricted)
        .with_approval(
            ApprovalPolicy::none()
                .requiring_role("Accounts Payable Clerk")
                .routed_to("role:cfo", "role:ceo", 3600),
        );
    // With a complete override the conflict is tolerated, and the approval still applies.
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::new("admin", ["override_sod"]),
    )
    .with_override(override_submission());
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    let GrantDecision::NeedsApproval(spec) = decision else {
        panic!("expected NeedsApproval, got {decision:#?}");
    };
    assert_eq!(spec.request, request_id("r1"));
    assert_eq!(spec.approver, "role:cfo");
    assert_eq!(
        spec.escalate_to.as_str(),
        "role:ceo",
        "a reported spec always carries a usable target"
    );
    assert_eq!(spec.sla_secs, 3600);
}

#[test]
fn a_policy_with_no_valid_escalation_target_is_refused_at_gate_time() {
    // WP-IAM-08.T6: the gate used to report NeedsApproval with an empty target and
    // leave the caller to find out. It now fails closed, naming the rule and the request.
    let gate = GrantGate::new();
    for policy in [
        // An administrator hand-builds a policy that requires approval and forgets the target.
        ApprovalPolicy::none().requiring_role("Accounts Payable Clerk"),
        // The lenient builder fails closed on a blank target, so the policy keeps none.
        ApprovalPolicy::none()
            .requiring_role("Accounts Payable Clerk")
            .routed_to("role:cfo", "", 3600),
    ] {
        let state = base()
            .with_delegation(DelegationPolicy::Unrestricted)
            .with_approval(policy);
        let req = request(
            "r1",
            "jane",
            "Accounts Payable Clerk",
            Some(scope(A)),
            Grantor::plain("admin"),
        );
        let err = gate
            .evaluate(&req, &state)
            .expect_err("an approval that escalates to nobody is refused");
        assert_eq!(err.code(), spot_iam_governance::codes::REQUEST_INVALID);
        assert_eq!(err.subject(), "r1", "the refusal names the request");
        let text = err.to_string();
        assert!(text.contains("escalation target"), "{text}");
    }
}

#[test]
fn a_gate_refusal_never_echoes_the_offered_target() {
    let gate = GrantGate::new();
    let state = base()
        .with_delegation(DelegationPolicy::Unrestricted)
        .with_approval(
            ApprovalPolicy::none()
                .requiring_role("Accounts Payable Clerk")
                .routed_to("role:cfo", "a\u{0}b", 3600),
        );
    let req = request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::plain("admin"),
    );
    let err = gate
        .evaluate(&req, &state)
        .expect_err("a control character is not a target");
    let text = err.to_string();
    assert!(
        !text.contains("a\u{0}b"),
        "the value must not be echoed: {text}"
    );
    assert!(!text.contains("\\u0000"), "{text}");
}

#[test]
fn the_request_source_is_validated_against_the_managed_roles() {
    // An emergency grant of a role the tenant declared IdP-managed is not a manual change, so
    // it is not refused by the IdP-managed rule.
    let gate = GrantGate::new();
    let mut managed = ManagedRoles::new();
    managed.declare(RoleName::from("Finance Approver"), "okta");
    let state = base()
        .with_delegation(DelegationPolicy::Unrestricted)
        .with_managed_roles(managed);
    let req = request(
        "r1",
        "jane",
        "Finance Approver",
        Some(scope(A)),
        Grantor::plain("admin"),
    )
    .with_source(Source::Emergency);
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    assert!(decision.is_allowed(), "{decision:#?}");
}

#[test]
fn a_user_cannot_override_a_conflict_on_their_own_access() {
    let gate = GrantGate::new();
    let req = request(
        "r-self",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(A)),
        Grantor::new("jane", ["override_sod"]),
    )
    .with_override(override_submission());
    let decision = gate
        .evaluate(&req, &conflict_state())
        .expect("the gate decides");
    assert!(
        matches!(decision, GrantDecision::Blocked(_)),
        "self-override must be blocked, got {decision:#?}"
    );
}

#[test]
fn a_request_with_a_blank_role_is_blocked_not_allowed() {
    let gate = GrantGate::new();
    let req = request(
        "r-blank",
        "jane",
        "  ",
        Some(scope(A)),
        Grantor::plain("admin"),
    );
    let decision = gate.evaluate(&req, &base()).expect("the gate decides");
    assert!(
        matches!(decision, GrantDecision::Blocked(_)),
        "a blank role must never be allowed, got {decision:#?}"
    );
}
