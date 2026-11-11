//! Barrier 3 of ADR-083 on the grant gate: manual grants, delegated administration, rule and
//! IdP sources, and the emergency grant all stop at the same refusal, and nothing lifts it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_iam_governance::{
    Conflict, DelegationPolicy, GovernanceState, GrantDecision, GrantGate, GrantRequest,
    GrantRequestId, Grantor,
};
use spot_iam_roles::{OutsideGuard, Registry, Source};

const FIXTURE: &str = include_str!("../../spot-iam-roles/tests/fixtures/outside.toml");

fn state(policy: DelegationPolicy) -> GovernanceState {
    let registry = Registry::from_toml(FIXTURE).unwrap_or_else(|e| panic!("{e}"));
    GovernanceState::new(OutsideGuard::new(&registry, ["portal-1"])).with_delegation(policy)
}

fn request(user: &str, role: &str, source: Source, grantor: Grantor) -> GrantRequest {
    GrantRequest::new(&GrantRequestId::new("r1").unwrap(), user, role, grantor).with_source(source)
}

fn codes(decision: &GrantDecision) -> Vec<String> {
    match decision {
        GrantDecision::Blocked(conflicts) => conflicts
            .iter()
            .map(|c| match c {
                Conflict::Refusal(r) => r.code.clone(),
                Conflict::Sod(s) => s.rule_id.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn every_source_and_policy_stops_a_portal_user_at_the_same_refusal() {
    let gate = GrantGate::new();
    let sources = [Source::Manual, Source::Rule, Source::Idp, Source::Emergency];
    for policy in [DelegationPolicy::Unrestricted, DelegationPolicy::WithinHeld] {
        for source in sources {
            // (user, role, expected refusal code or None when the gate allows)
            let table = [
                ("portal-1", "Desk Clerk", Some("iam_outside.role_kind")),
                (
                    "portal-1",
                    "Portal Leaky",
                    Some("iam_outside.duty_not_exposed"),
                ),
                (
                    "portal-1",
                    "Portal Inherits Leaky",
                    Some("iam_outside.duty_not_exposed"),
                ),
                (
                    "portal-1",
                    "Portal Direct",
                    Some("iam_outside.duty_not_exposed"),
                ),
                ("portal-1", "No Such Role", Some("iam_outside.role_kind")),
                ("staff-1", "Desk Clerk", None),
            ];
            for (user, role, expected) in table {
                let req = request(user, role, source, Grantor::plain("admin"));
                let decision = gate.evaluate(&req, &state(policy)).unwrap();
                match expected {
                    Some(code) => assert_eq!(
                        codes(&decision),
                        [code],
                        "{policy:?} {source:?} {role}: {decision:?}"
                    ),
                    // For an internal user the outside rule is silent; whatever else the policy
                    // says (the grantor holds nothing) is not this rule's.
                    None => assert!(
                        !codes(&decision)
                            .iter()
                            .any(|c| c.starts_with("iam_outside.")),
                        "{policy:?} {source:?} {role}: {decision:?}"
                    ),
                }
            }
        }
    }
}

#[test]
fn a_portal_role_with_only_exposed_duties_is_allowed() {
    let req = request(
        "portal-1",
        "Portal Viewer",
        Source::Manual,
        Grantor::plain("admin"),
    );
    let decision = GrantGate::new()
        .evaluate(&req, &state(DelegationPolicy::Unrestricted))
        .unwrap();
    assert!(decision.is_allowed(), "{decision:?}");
}

#[test]
fn an_override_right_does_not_lift_the_refusal() {
    let req = request(
        "portal-1",
        "Desk Clerk",
        Source::Manual,
        Grantor::new("admin", ["override_sod"]),
    );
    let decision = GrantGate::new()
        .evaluate(&req, &state(DelegationPolicy::Unrestricted))
        .unwrap();
    assert_eq!(codes(&decision), ["iam_outside.role_kind"]);
    for conflict in decision.conflicts() {
        assert!(!conflict.is_overridable());
    }
}

#[test]
fn the_refusal_names_the_role_and_never_the_user() {
    let req = request(
        "portal-1",
        "Portal Leaky",
        Source::Manual,
        Grantor::plain("admin"),
    );
    let decision = GrantGate::new()
        .evaluate(&req, &state(DelegationPolicy::Unrestricted))
        .unwrap();
    let GrantDecision::Blocked(conflicts) = decision else {
        panic!("blocked");
    };
    let Conflict::Refusal(refusal) = &conflicts[0] else {
        panic!("a refusal");
    };
    assert!(refusal.detail.contains("post_invoices"));
    assert!(refusal.subject == "Portal Leaky");
    assert!(!refusal.detail.contains("portal-1"));
}

#[test]
fn the_explicit_no_portal_state_protects_no_one() {
    let req = request(
        "portal-1",
        "Desk Clerk",
        Source::Manual,
        Grantor::plain("admin"),
    );
    let state =
        GovernanceState::without_portal_users().with_delegation(DelegationPolicy::Unrestricted);
    let decision = GrantGate::new().evaluate(&req, &state).unwrap();
    assert!(decision.is_allowed());
}
