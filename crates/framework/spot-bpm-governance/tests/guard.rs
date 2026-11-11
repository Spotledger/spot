//! The guard's decision matrix: free, governed (in case / outside process / refused) and
//! process_only, with the audit of every exception and refusal.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::context::Actor;
use spot_app_api::prelude::RecordId;
use spot_bpm_governance::{
    AuditKind, CaseContext, CreateAttempt, FailingAudit, GovernanceDecision, GovernanceError,
    GovernanceGuard, GovernanceMode, MemoryPolicyStore, MemoryPrincipals, MemoryRequests,
    NoRequests, OUTSIDE_PROCESS_PERMISSION, PolicyChange, PolicyStore, RecordingAudit,
    RefusalReason, RoleOutsideProcess, Route, VerifiedRequest,
};
use spot_permissions::Principal;

/// A create attempt with the shared defaults.
fn attempt<'a>(
    company: &'a str,
    doctype: &'a str,
    actor: &'a Actor,
    principal: &'a Principal,
    reason: Option<&'a str>,
    request: Option<&'a VerifiedRequest>,
) -> CreateAttempt<'a> {
    CreateAttempt {
        company,
        doctype,
        document_id: RecordId::new_v7(),
        actor,
        principal,
        reason,
        request,
        command_id: Some("command-1"),
        now: now(),
    }
}

/// A step the authority would have verified: bound to a request, a node and an attempt.
fn verified(request: &str, node: &str, attempt: u32) -> VerifiedRequest {
    VerifiedRequest {
        request: request.to_owned(),
        node: node.to_owned(),
        attempt,
    }
}

fn ports() -> Ports {
    Ports {
        policy: MemoryPolicyStore::new(8).shared(),
        requests: MemoryRequests::new().shared(),
        audit: RecordingAudit::new().shared(),
    }
}

#[test]
fn free_is_allowed_and_not_audited() {
    let ports = ports();
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = plain_principal();
    let decision = guard
        .before_create(&attempt("acme", "note", &actor, &principal, None, None))
        .unwrap();
    assert_eq!(decision, GovernanceDecision::allow(Route::Free));
    assert!(ports.audit.is_empty(), "a free create is not audited");
}

#[test]
fn governed_without_case_or_permission_is_refused_and_audited() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = plain_principal();
    let decision = guard
        .before_create(&attempt("acme", "note", &actor, &principal, None, None))
        .unwrap();
    assert_eq!(
        decision,
        GovernanceDecision::Refuse {
            reason: RefusalReason::NoPermission
        }
    );
    let entries = ports.audit.entries();
    assert_eq!(entries.len(), 1, "a refusal is audited");
    assert_eq!(entries[0].kind, AuditKind::Refused);
    assert_eq!(entries[0].company, "acme");
    assert_eq!(entries[0].mode, GovernanceMode::Governed);
}

#[test]
fn governed_in_case_is_allowed_without_audit() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    ports.requests.allow("case-1", "node-1", "note");
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = plain_principal();
    let request = verified("case-1", "node-1", 1);
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            None,
            Some(&request),
        ))
        .unwrap();
    assert_eq!(decision, GovernanceDecision::allow(Route::InCase));
    assert!(
        ports.audit.is_empty(),
        "the in-case route is not an exception"
    );
}

#[test]
fn governed_case_that_does_not_permit_is_refused() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    ports.requests.allow("case-1", "node-1", "customer");
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = plain_principal();
    let request = verified("case-1", "node-1", 1);
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            None,
            Some(&request),
        ))
        .unwrap();
    assert_eq!(
        decision,
        GovernanceDecision::Refuse {
            reason: RefusalReason::NotInCase
        }
    );
    assert_eq!(ports.audit.len(), 1);
}

#[test]
fn outside_process_with_reason_is_allowed_and_audited() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = outside_principal();
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            Some("approved by the controller"),
            None,
        ))
        .unwrap();
    assert_eq!(decision, GovernanceDecision::allow(Route::OutsideProcess));
    let entries = ports.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, AuditKind::OutsideProcess);
    assert_eq!(
        entries[0].reason.as_deref(),
        Some("approved by the controller"),
        "who, what, why and which policy"
    );
    assert_eq!(entries[0].doctype.as_deref(), Some("note"));
}

#[test]
fn blank_reason_is_refused() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = outside_principal();
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            Some("   \t"),
            None,
        ))
        .unwrap();
    assert_eq!(
        decision,
        GovernanceDecision::Refuse {
            reason: RefusalReason::ReasonRequired { min_chars: 8 }
        }
    );
    assert_eq!(ports.audit.len(), 1, "a refused exception is audited");
}

#[test]
fn short_reason_is_refused_at_the_configured_floor() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    ports.policy.set_min_reason_chars("acme", 20);
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = outside_principal();
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            Some("too short"),
            None,
        ))
        .unwrap();
    assert_eq!(
        decision,
        GovernanceDecision::Refuse {
            reason: RefusalReason::ReasonRequired { min_chars: 20 }
        }
    );
}

#[test]
fn the_system_does_not_hold_the_permission() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    let guard = guard(&ports);
    let actor = import_actor();
    // A system principal, even with a reason, is not an outside-process exception.
    let principal = Principal::system();
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            Some("a perfectly long reason"),
            None,
        ))
        .unwrap();
    assert_eq!(
        decision,
        GovernanceDecision::Refuse {
            reason: RefusalReason::NoPermission
        }
    );
}

#[test]
fn process_only_defers_with_no_bypass() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::ProcessOnly);
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = outside_principal();
    // Even holding the permission and a reason, there is no bypass in process_only.
    let decision = guard
        .before_create(&attempt(
            "acme",
            "note",
            &actor,
            &principal,
            Some("a perfectly long reason"),
            None,
        ))
        .unwrap();
    assert_eq!(
        decision,
        GovernanceDecision::Defer {
            to: spot_bpm_governance::DeferredTo::Entry
        }
    );
    assert!(ports.audit.is_empty(), "process_only is enforced elsewhere");
}

#[test]
fn another_company_is_unaffected() {
    let ports = ports();
    ports
        .policy
        .set_mode("acme", "note", GovernanceMode::Governed);
    let guard = guard(&ports);
    let actor = user_actor();
    let principal = plain_principal();
    let decision = guard
        .before_create(&attempt("other", "note", &actor, &principal, None, None))
        .unwrap();
    assert_eq!(decision, GovernanceDecision::allow(Route::Free));
    assert!(ports.audit.is_empty());
}

#[test]
fn audit_failure_fails_closed() {
    let policy = MemoryPolicyStore::new(8).shared();
    policy.set_mode("acme", "note", GovernanceMode::Governed);
    let guard = GovernanceGuard::new(
        policy,
        Arc::new(NoRequests),
        Arc::new(RoleOutsideProcess),
        Arc::new(FailingAudit),
    );
    let actor = user_actor();
    let principal = plain_principal();
    let error = guard
        .before_create(&attempt("acme", "note", &actor, &principal, None, None))
        .unwrap_err();
    assert!(matches!(error, GovernanceError::Audit { .. }));
}

#[test]
fn a_policy_change_is_recorded_and_audited() {
    let ports = ports();
    let guard = guard(&ports);
    let actor = user_actor();
    let change = PolicyChange::new(
        "acme",
        "note",
        GovernanceMode::Free,
        GovernanceMode::Governed,
        later(3600),
        "admin",
        now(),
    );
    guard.change_policy(change.clone(), &actor).unwrap();
    assert_eq!(ports.policy.changes("acme", "note").unwrap(), vec![change]);
    let entries = ports.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, AuditKind::PolicyChange);
    assert_eq!(entries[0].mode, GovernanceMode::Governed);
}

#[test]
fn case_context_round_trips_through_the_wire_form() {
    let case = CaseContext::new("case-1", "node-1", 3);
    assert_eq!(CaseContext::parse(&case.encode()), Some(case));
    assert_eq!(CaseContext::parse("not-a-key"), None);
}

#[test]
fn the_permission_name_is_the_contract() {
    assert_eq!(OUTSIDE_PROCESS_PERMISSION, "bpm.outside_process");
}

/// The memory principal source resolves a known user and the system.
#[test]
fn principal_source_resolves_the_system() {
    let source = MemoryPrincipals::new();
    source.insert(user_id(), plain_principal());
    let source = Arc::new(source);
    use spot_bpm_governance::PrincipalSource;
    assert!(source.principal(&import_actor()).is_system);
    assert_eq!(source.principal(&user_actor()).user, user_id().to_string());
}
