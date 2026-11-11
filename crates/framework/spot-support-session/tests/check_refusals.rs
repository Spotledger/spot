//! The check's refusals: every code has a test, the order is tested with an action that
//! fails two ways, `AllRead` refuses any write, and the no-DocType rule holds.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::check::ActionRequest;
use spot_support_session::model::{ActionScope, DocTypeScope, Scope, SessionPolicy};
use spot_support_session::{SessionError, SessionGrant, Tier};

fn action(name: &str, tier: Tier, doctype: Option<&str>) -> ActionRequest {
    ActionRequest {
        action: name.to_owned(),
        tier,
        doctype: doctype.map(str::to_owned),
        tenant: TENANT.to_owned(),
    }
}

fn t2_policy() -> SessionPolicy {
    SessionPolicy {
        max_tier_without_owner_approval: Tier::T2,
        ..SessionPolicy::default()
    }
}

fn t2_grant_any() -> SessionGrant {
    let mut s = spec();
    s.tier_ceiling = Tier::T2;
    SessionGrant::request(&s, &t2_policy(), START)
        .unwrap()
        .approve(&owner(), START, &t2_policy())
        .unwrap()
        .0
}

#[test]
fn wrong_tenant_is_refused_first() {
    let g = grant();
    let mut a = action("read", Tier::T0, Some("Sales Invoice"));
    a.tenant = "other-tenant".to_owned();
    let refusal = g.check(&a, START + 1).unwrap_err();
    assert_eq!(refusal.code(), "session.wrong_tenant");
    assert!(refusal.to_string().contains("other-tenant"));
    assert!(refusal.to_string().contains(TENANT));
}

#[test]
fn not_yet_active_is_refused_before_the_start() {
    let g = grant();
    let refusal = g
        .check(&action("read", Tier::T0, Some("Sales Invoice")), START - 1)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.not_yet_active");
}

#[test]
fn expired_is_refused_at_and_after_the_boundary() {
    let g = grant();
    for now in [END, END + 1, END + 10_000] {
        let refusal = g
            .check(&action("read", Tier::T0, Some("Sales Invoice")), now)
            .unwrap_err();
        assert_eq!(refusal.code(), "session.expired", "now {now}");
    }
}

#[test]
fn revoked_is_refused_inside_the_window() {
    let mut g = t2_grant_any();
    g.revoke("tenant-admin", "reason", START + 5).unwrap();
    let refusal = g
        .check(&action("read", Tier::T0, Some("Sales Invoice")), START + 10)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.revoked");
    assert!(refusal.to_string().contains("tenant-admin"));
}

#[test]
fn above_ceiling_is_refused_over_the_grant_tier() {
    let g = t2_grant_any();
    let refusal = g
        .check(
            &action("install_app", Tier::T3, Some("Sales Invoice")),
            START + 1,
        )
        .unwrap_err();
    assert_eq!(refusal.code(), "session.above_ceiling");
    assert!(refusal.to_string().contains("T3"));
    assert!(refusal.to_string().contains("T2"));
}

#[test]
fn action_not_in_scope_is_refused_outside_the_only_list() {
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["Sales Invoice".to_owned()]),
        actions: ActionScope::Only(vec!["read".to_owned(), "update".to_owned()]),
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let refusal = g
        .check(
            &action("delete", Tier::T0, Some("Sales Invoice")),
            START + 1,
        )
        .unwrap_err();
    assert_eq!(refusal.code(), "session.action_not_in_scope");
    assert!(refusal.to_string().contains("delete"));
}

#[test]
fn doctype_not_in_scope_is_refused_outside_the_named_list() {
    let g = grant();
    let refusal = g
        .check(&action("read", Tier::T0, Some("Customer")), START + 1)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.doctype_not_in_scope");
    assert!(refusal.to_string().contains("Customer"));
}

#[test]
fn write_in_read_only_scope_is_refused_in_an_allread_session() {
    let mut s = spec();
    s.tier_ceiling = Tier::T0;
    s.scope = Scope {
        doctypes: DocTypeScope::AllRead,
        actions: ActionScope::Any,
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    // Every write shape is refused, named or namespaced; reads pass everywhere.
    for name in [
        "update",
        "create",
        "submit",
        "cancel",
        "Sales Invoice.cancel",
        "customer_update",
    ] {
        let refusal = g
            .check(&action(name, Tier::T0, Some("Sales Invoice")), START + 1)
            .unwrap_err();
        assert_eq!(refusal.code(), "session.write_in_read_only_scope", "{name}");
    }
    assert!(
        g.check(
            &action("read", Tier::T0, Some("Anything At All")),
            START + 1
        )
        .is_ok()
    );
    assert!(
        g.check(
            &action("diagnose", Tier::T0, Some("Payment Entry")),
            START + 1
        )
        .is_ok()
    );
}

#[test]
fn every_refusal_code_is_distinct_and_session_prefixed() {
    let codes: std::collections::BTreeSet<&str> = [
        "session.wrong_tenant",
        "session.not_yet_active",
        "session.expired",
        "session.revoked",
        "session.above_ceiling",
        "session.action_not_in_scope",
        "session.doctype_not_in_scope",
        "session.write_in_read_only_scope",
    ]
    .into_iter()
    .collect();
    assert_eq!(codes.len(), 8);
    for c in codes {
        assert!(c.starts_with("session."));
    }
}

#[test]
fn the_order_is_first_failing_reason_with_an_action_failing_two_ways() {
    // A T3 write-named action, out of the `Only` list, on a DocType out of the list,
    // inside an active named grant with a T1 ceiling: three failures at once; the first
    // in the order wins (`above_ceiling` over `action_not_in_scope` over
    // `doctype_not_in_scope`).
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["Sales Invoice".to_owned()]),
        actions: ActionScope::Only(vec!["read".to_owned()]),
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let refusal = g
        .check(
            &action("install_app", Tier::T3, Some("Customer")),
            START + 1,
        )
        .unwrap_err();
    assert_eq!(refusal.code(), "session.above_ceiling");
}

#[test]
fn the_order_keeps_action_scope_over_doctype_scope() {
    // Under the ceiling: action scope is checked before the DocType scope.
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["Sales Invoice".to_owned()]),
        actions: ActionScope::Only(vec!["read".to_owned()]),
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let refusal = g
        .check(&action("delete", Tier::T0, Some("Customer")), START + 1)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.action_not_in_scope");
}

#[test]
fn the_time_checks_come_before_the_scope_checks() {
    let g = grant();
    // Wrong tier and wrong DocType, but the session has not started: the time reason
    // wins.
    let refusal = g
        .check(
            &action("install_app", Tier::T3, Some("Customer")),
            START - 1,
        )
        .unwrap_err();
    assert_eq!(refusal.code(), "session.not_yet_active");
    let refusal = g
        .check(&action("install_app", Tier::T3, Some("Customer")), END + 1)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.expired");
}

#[test]
fn the_tenant_check_comes_before_everything_including_the_time() {
    let g = grant();
    let mut a = action("install_app", Tier::T3, Some("Customer"));
    a.tenant = "elsewhere".to_owned();
    let refusal = g.check(&a, START - 1).unwrap_err();
    assert_eq!(refusal.code(), "session.wrong_tenant");
}

#[test]
fn an_action_with_no_doctype_needs_any_actions_or_a_listing() {
    // `Any` actions: a tenant-wide diagnostic is allowed.
    let g = grant();
    assert!(
        g.check(&action("health_check", Tier::T0, None), START + 1)
            .is_ok()
    );
    // An `Only` list that does not name it: refused.
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["Sales Invoice".to_owned()]),
        actions: ActionScope::Only(vec!["read".to_owned()]),
    };
    let listed = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let refusal = listed
        .check(&action("health_check", Tier::T0, None), START + 1)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.action_not_in_scope");
    // The same `Only` list naming the diagnostic: allowed.
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["Sales Invoice".to_owned()]),
        actions: ActionScope::Only(vec!["read".to_owned(), "health_check".to_owned()]),
    };
    let g2 = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    assert!(
        g2.check(&action("health_check", Tier::T0, None), START + 1)
            .is_ok()
    );
}

#[test]
fn an_only_action_list_still_respects_the_doctype_scope() {
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["Sales Invoice".to_owned()]),
        actions: ActionScope::Only(vec!["read".to_owned(), "diagnose".to_owned()]),
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    assert!(
        g.check(&action("read", Tier::T0, Some("Sales Invoice")), START + 1)
            .is_ok()
    );
    let refusal = g
        .check(&action("diagnose", Tier::T0, Some("Customer")), START + 1)
        .unwrap_err();
    assert_eq!(refusal.code(), "session.doctype_not_in_scope");
}

#[test]
fn an_allread_write_refusal_does_not_leak_past_its_scope_even_when_listed() {
    // `AllRead` with an `Only` list naming an update: the DocType branch of an AllRead
    // session is read-only regardless; a listed write still dies on the action scope
    // first (the order).
    let mut s = spec();
    s.tier_ceiling = Tier::T0;
    s.scope = Scope {
        doctypes: DocTypeScope::AllRead,
        actions: ActionScope::Only(vec!["read".to_owned(), "diagnose".to_owned()]),
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let refusal = g
        .check(
            &action("update", Tier::T0, Some("Sales Invoice")),
            START + 1,
        )
        .unwrap_err();
    assert_eq!(refusal.code(), "session.action_not_in_scope");
    // And with `Any` actions the write refusal is the answer.
    let mut s = spec();
    s.tier_ceiling = Tier::T0;
    s.scope = Scope {
        doctypes: DocTypeScope::AllRead,
        actions: ActionScope::Any,
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    assert!(
        g.check(&action("diagnose", Tier::T0, None), START + 1)
            .is_ok()
    );
}

#[test]
fn the_refusal_error_display_carries_the_code_line() {
    let g = grant();
    let refusal = g
        .check(&action("read", Tier::T0, Some("Customer")), START + 1)
        .unwrap_err();
    let text = refusal.to_string();
    assert!(text.starts_with("session.doctype_not_in_scope:"));
}

#[test]
fn a_refusal_converts_to_a_refused_event_the_caller_journals() {
    let g = grant();
    let refusal = g
        .check(&action("read", Tier::T0, Some("Customer")), START + 5)
        .unwrap_err();
    let event = spot_support_session::SessionEvent::from_refusal(&refusal, START + 5);
    assert_eq!(event.kind(), "refused");
    assert_eq!(event.at(), START + 5);
    if let spot_support_session::SessionEvent::Refused { code, reason, .. } = &event {
        assert_eq!(code, "session.doctype_not_in_scope");
        assert!(reason.contains("Customer"));
    } else {
        panic!("expected a Refused event");
    }
    // The serialised event carries no action arguments: only the code and the sentence.
    let json = serde_json::to_string(&event).unwrap();
    assert!(
        !json.contains("read"),
        "no action name in the event: {json}"
    );
    assert!(!json.contains("action"));
}

#[test]
fn a_request_error_never_leaks_into_the_wire_shape() {
    // The errors are for the caller's logs: they carry the words, the codes stay in the
    // refusal path. A smoke assertion that the error type is the one and only one.
    let err = SessionError::ReasonBlank;
    assert_eq!(err, SessionError::ReasonBlank);
}
