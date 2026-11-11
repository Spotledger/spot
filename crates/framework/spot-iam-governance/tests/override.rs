//! The recorded override: it is complete, it covers exactly one scope, and it is the note an
//! access review shows.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assignment, catalogue, request, rules};
use spot_access_review::{
    AssignmentRef, Campaign, CampaignSpec, DeadlinePolicy, OverrideNote, ReviewItem,
};
use spot_iam_governance::{
    Conflict, DelegationPolicy, GovernanceState, GrantDecision, GrantGate, GrantScope, Grantor,
    MitigatingControl, OverrideSubmission, ScopeHierarchy,
};

const A: &str = "Company A";
const B: &str = "Company B";

fn scope(value: &str) -> GrantScope {
    GrantScope::value("Company", value)
}

fn state(held_scope: &str) -> GovernanceState {
    GovernanceState::without_portal_users()
        .with_rules(rules())
        .with_catalogue(catalogue())
        .with_assignments(vec![assignment(
            "jane",
            "Finance Approver",
            Some(scope(held_scope)),
        )])
        .with_delegation(DelegationPolicy::Unrestricted)
        .with_hierarchy(ScopeHierarchy::new())
}

fn request_for(scope_value: &str) -> spot_iam_governance::GrantRequest {
    request(
        "r1",
        "jane",
        "Accounts Payable Clerk",
        Some(scope(scope_value)),
        Grantor::new("admin", ["override_sod"]),
    )
    .with_override(OverrideSubmission::new(
        "month-end close only",
        MitigatingControl::new("C-1", "second reviewer"),
    ))
}

#[test]
fn the_override_record_is_the_access_review_input() {
    let gate = GrantGate::new();
    let state_a = state(A);
    let req = request_for(A);
    assert!(
        gate.evaluate(&req, &state_a)
            .expect("the gate decides")
            .is_allowed()
    );
    let records = gate
        .record_overrides(&req, &gate.sod_conflicts(&req, &state_a), 100)
        .expect("records");
    assert_eq!(records.len(), 2);

    let item = ReviewItem {
        id: "item-1".to_owned(),
        user: "jane".to_owned(),
        assignment: AssignmentRef::new("grant:r1"),
        source: "manual".to_owned(),
        last_use: None,
        sod_override: Some(OverrideNote::new(records[0].access_review_note())),
        reviewer: "manager".to_owned(),
    };
    let spec = CampaignSpec {
        id: "campaign-1".to_owned(),
        title: "Quarterly access review".to_owned(),
        opened_by: "admin".to_owned(),
        escalation_contact: "cfo".to_owned(),
        deadline: 1_000,
        stale_after_days: 90,
        on_deadline: DeadlinePolicy::LeaveOpen,
    };
    let campaign = Campaign::open(spec, vec![item], 0).expect("campaign opens");
    assert!(
        campaign.items()[0].is_overridden(),
        "the review shows the override"
    );
    assert!(
        campaign.items()[0]
            .sod_override
            .as_ref()
            .expect("override note")
            .as_str()
            .contains("second reviewer")
    );
}

#[test]
fn an_override_covers_exactly_the_scope_it_names() {
    let gate = GrantGate::new();
    let state_a = state(A);
    let req_a = request_for(A);
    let records = gate
        .record_overrides(&req_a, &gate.sod_conflicts(&req_a, &state_a), 100)
        .expect("records");

    // A later grant in Company A with no new submission is covered by the recorded override.
    let covered = spot_iam_governance::GrantRequest::new(
        &common::request_id("r2"),
        "jane",
        "Accounts Payable Clerk",
        Grantor::new("admin", ["override_sod"]),
    )
    .with_scope(scope(A));
    let state_a = state(A).with_overrides(records.clone());
    assert!(
        gate.evaluate(&covered, &state_a)
            .expect("the gate decides")
            .is_allowed(),
        "the recorded override covers its own scope"
    );

    // The same conflict in Company B is not covered by an override recorded for Company A.
    let uncovered = spot_iam_governance::GrantRequest::new(
        &common::request_id("r3"),
        "jane",
        "Accounts Payable Clerk",
        Grantor::new("admin", ["override_sod"]),
    )
    .with_scope(scope(B));
    let state_b = state(B).with_overrides(records);
    let decision = gate
        .evaluate(&uncovered, &state_b)
        .expect("the gate decides");
    let GrantDecision::NeedsOverride(conflicts) = decision else {
        panic!("expected NeedsOverride, got {decision:#?}");
    };
    assert_eq!(conflicts.len(), 2);
    assert!(conflicts.iter().all(|c| matches!(c, Conflict::Sod(_))));
}

#[test]
fn a_refusal_is_never_overridable() {
    let refusal = Conflict::Refusal(spot_iam_governance::Refusal::new(
        spot_iam_governance::codes::DELEGATION_DENIED,
        "role",
        "not held",
    ));
    assert!(!refusal.is_overridable());
}
