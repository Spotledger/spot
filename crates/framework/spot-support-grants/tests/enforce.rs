//! Enforcement: the scope matrix, the exact expiry instant, revocation during a session,
//! and denials recorded on the customer side.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use spot_support_grants::{Action, ControlPlane, CustomerLog, Decision, SessionRef};
use spot_support_session::Tier;

fn denial_code(h: &Harness, session: &SessionRef, action: &Action, now: i64) -> String {
    match spot_support_grants::check(&h.grants, session, action, now) {
        Decision::Allow => "allow".to_owned(),
        Decision::Deny(denial) => denial.code.to_owned(),
    }
}

#[test]
fn the_scope_matrix_allows_only_what_the_customer_approved() {
    let mut h = Harness::new();
    h.grant(
        "g-scope",
        Tier::T1,
        scope(&["Factura", "Payment"], &["read", "update"]),
        100,
        200,
    );
    let grant_session = session("g-scope");

    assert_eq!(
        denial_code(&h, &grant_session, &read("Factura"), 150),
        "allow"
    );
    assert_eq!(
        denial_code(&h, &grant_session, &read("Other"), 150),
        "session.doctype_not_in_scope"
    );
    assert_eq!(
        denial_code(
            &h,
            &grant_session,
            &action("update", Tier::T1, Some("Payment")),
            150
        ),
        "allow"
    );
    assert_eq!(
        denial_code(
            &h,
            &grant_session,
            &action("create", Tier::T1, Some("Factura")),
            150
        ),
        "session.action_not_in_scope"
    );
    assert_eq!(
        denial_code(
            &h,
            &grant_session,
            &action("read", Tier::T2, Some("Factura")),
            150
        ),
        "session.above_ceiling"
    );

    // Another tenant, another support organisation, and an unknown grant.
    let mut other_tenant = action("read", Tier::T0, Some("Factura"));
    other_tenant.tenant = "tenant-b".to_owned();
    assert_eq!(
        denial_code(&h, &grant_session, &other_tenant, 150),
        "support_grants.wrong_tenant"
    );
    let mut wrong_org = grant_session.clone();
    wrong_org.support_org = "another-org".to_owned();
    assert_eq!(
        denial_code(&h, &wrong_org, &read("Factura"), 150),
        "support_grants.support_org_mismatch"
    );
    let mut wrong_subject = grant_session.clone();
    wrong_subject.subject_id = "another-agent".to_owned();
    assert_eq!(
        denial_code(&h, &wrong_subject, &read("Factura"), 150),
        "support_grants.subject_mismatch"
    );
    assert_eq!(
        denial_code(&h, &session("g-none"), &read("Factura"), 150),
        "support_grants.unknown_grant"
    );

    // A read-only session may read anywhere but never write.
    h.grant("g-read", Tier::T0, all_read(), 100, 200);
    let read_only = session("g-read");
    assert_eq!(
        denial_code(&h, &read_only, &read("AnyDocType"), 150),
        "allow"
    );
    assert_eq!(
        denial_code(
            &h,
            &read_only,
            &action("update", Tier::T0, Some("AnyDocType")),
            150
        ),
        "session.write_in_read_only_scope"
    );
}

#[test]
fn expiry_is_enforced_at_the_exact_instant() {
    let mut h = Harness::new();
    h.grant("g-exp", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    let grant_session = session("g-exp");

    assert_eq!(
        denial_code(&h, &grant_session, &read("Factura"), 99),
        "session.not_yet_active"
    );
    assert_eq!(
        denial_code(&h, &grant_session, &read("Factura"), 100),
        "allow"
    );
    assert_eq!(
        denial_code(&h, &grant_session, &read("Factura"), 199),
        "allow"
    );
    assert_eq!(
        denial_code(&h, &grant_session, &read("Factura"), 200),
        "session.expired"
    );
    assert_eq!(
        denial_code(&h, &grant_session, &read("Factura"), 201),
        "session.expired"
    );
}

#[test]
fn revocation_during_a_session_kills_it_immediately() {
    let mut h = Harness::new();
    h.grant(
        "g-rev",
        Tier::T1,
        scope(&["Factura"], &["read", "update"]),
        100,
        8000,
    );
    let session = session("g-rev");

    h.plane().start_session(TENANT, "g-rev", 100).unwrap();
    let allowed = h.plane().attempt(&session, &read("Factura"), 110);
    assert!(allowed.is_allowed());

    h.plane()
        .revoke(
            TENANT,
            "g-rev",
            ADMIN,
            "the customer ended the session",
            120,
        )
        .unwrap();

    let denied = h.plane().attempt(&session, &read("Factura"), 121);
    assert_eq!(denied.code(), "session.revoked");
    let error = h
        .plane()
        .start_session(TENANT, "g-rev", 130)
        .expect_err("a revoked grant cannot start");
    assert_eq!(error.code(), spot_support_grants::codes::NOT_RUNNING);
    assert_eq!(
        h.plane()
            .revoke(TENANT, "g-rev", ADMIN, "again", 131)
            .unwrap_err()
            .code(),
        spot_support_grants::codes::ALREADY_REVOKED
    );

    // The revocation is in the customer's record, and the denied action after it too.
    let sessions = h.sessions();
    let record = sessions.iter().find(|s| s.session == "g-rev").unwrap();
    assert_eq!(record.revoked_at, Some(120));
    assert_eq!(record.revocation_reason, "the customer ended the session");
    assert!(
        record
            .actions
            .iter()
            .any(|a| a.outcome == "denied" && a.code == "session.revoked")
    );
}

#[test]
fn out_of_scope_and_expired_attempts_are_logged_on_the_customer_side() {
    let mut h = Harness::new();
    h.grant("g-log", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    let session = session("g-log");

    h.plane().start_session(TENANT, "g-log", 100).unwrap();
    assert!(
        h.plane()
            .attempt(&session, &read("Factura"), 110)
            .is_allowed()
    );
    assert_eq!(
        h.plane().attempt(&session, &read("Secret"), 120).code(),
        "session.doctype_not_in_scope"
    );
    assert_eq!(
        h.plane().attempt(&session, &read("Factura"), 250).code(),
        "session.expired"
    );
    h.plane()
        .stop_session(TENANT, "g-log", "done", 260)
        .unwrap();

    let sessions = h.sessions();
    let record = sessions.iter().find(|s| s.session == "g-log").unwrap();
    assert_eq!(record.actions.len(), 3, "every attempt is recorded");
    assert_eq!(record.actions[0].outcome, "allowed");
    assert_eq!(record.actions[0].code, "support_grants.allowed");
    assert_eq!(record.actions[1].outcome, "denied");
    assert_eq!(record.actions[1].code, "session.doctype_not_in_scope");
    assert_eq!(record.actions[2].code, "session.expired");
    assert_eq!(record.started_at, Some(100));
    assert_eq!(record.stopped_at, Some(260));
    assert!(record.chain_verified);
}

#[test]
fn an_allowed_action_that_cannot_be_recorded_is_refused() {
    let mut h = Harness::new();
    h.grant("g-fail", Tier::T1, scope(&["Factura"], &["read"]), 100, 200);
    let session = session("g-fail");
    let mut log = FailingLog::new(TENANT);
    let mut plane = ControlPlane::new(&mut h.grants, &mut log, &h.admins, &h.agents, &h.policy);
    let decision = plane.attempt(&session, &read("Factura"), 150);
    assert_eq!(
        decision.code(),
        spot_support_grants::codes::AUDIT_UNAVAILABLE
    );
    assert!(!decision.is_allowed());
    // The chain writer refuses another tenant's events outright.
    let mut event = spot_support_grants::GrantEvent {
        kind: spot_support_grants::EventKind::Action,
        tenant: "tenant-b".to_owned(),
        session: "g-fail".to_owned(),
        who: AGENT.to_owned(),
        who_kind: spot_support_session::SubjectKind::Agent,
        subject: AGENT.to_owned(),
        subject_kind: spot_support_session::SubjectKind::Agent,
        support_org: ORG.to_owned(),
        tier: Tier::T0,
        scope: String::new(),
        action: "read".to_owned(),
        doctype: None,
        outcome: "allowed".to_owned(),
        code: "support_grants.allowed".to_owned(),
        change: String::new(),
        reason: String::new(),
        extended_to: None,
        at: 1,
    };
    // A real chain writer over the fixture's chain refuses a foreign tenant.
    let mut wrong = h.log;
    assert_eq!(
        wrong.append(event.clone()).unwrap_err().code(),
        spot_support_grants::codes::WRONG_TENANT
    );
    event.tenant = TENANT.to_owned();
    wrong
        .append(event)
        .expect("the tenant's own event is appended");
}
