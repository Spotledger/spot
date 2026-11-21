//! Request validation: each rejection the brief lists, and the acceptance paths around
//! them.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::model::{
    ActionScope, DocTypeScope, Scope, SessionPolicy, Subject, SubjectKind,
};
use spot_support_session::{SessionError, SessionGrant, Tier};

fn policy() -> SessionPolicy {
    SessionPolicy::default()
}

#[test]
fn a_valid_request_is_pending_and_carries_the_requested_event() {
    let pending = SessionGrant::request(&spec(), &policy(), START).unwrap();
    assert_eq!(pending.id(), "ss-0001");
    assert_eq!(
        pending.grant().status,
        spot_support_session::SessionStatus::Pending
    );
    let event = pending.event();
    assert_eq!(event.kind(), "requested");
    assert_eq!(event.actor(), "support-desk");
    assert_eq!(event.at(), START);
    assert_eq!(event.reason(), "customer reports stuck invoices");
}

#[test]
fn a_blank_reason_is_refused() {
    for reason in ["", "   ", "\t\n"] {
        let mut s = spec();
        s.reason = reason.to_owned();
        let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
        assert_eq!(err, SessionError::ReasonBlank);
    }
}

#[test]
fn an_empty_named_doctype_scope_is_refused() {
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec![]),
        actions: ActionScope::Any,
    };
    let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
    assert_eq!(err, SessionError::ScopeEmpty("doctypes"));
}

#[test]
fn a_named_doctype_scope_of_only_blanks_is_refused() {
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec!["  ".to_owned()]),
        actions: ActionScope::Any,
    };
    let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
    assert_eq!(err, SessionError::ScopeEmpty("doctypes"));
}

#[test]
fn an_end_before_the_start_is_refused() {
    let mut s = spec();
    s.starts_at = END;
    s.expires_at = START;
    let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
    assert!(matches!(err, SessionError::EndBeforeStart { .. }));
}

#[test]
fn an_end_equal_to_the_start_is_refused() {
    let mut s = spec();
    s.expires_at = s.starts_at;
    let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
    assert!(matches!(err, SessionError::EndBeforeStart { .. }));
}

#[test]
fn a_duration_over_the_policy_is_refused() {
    let mut s = spec();
    s.expires_at = s.starts_at + 4 * 3600 + 1;
    let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
    assert_eq!(
        err,
        SessionError::DurationOverPolicy {
            asked: 4 * 3600 + 1,
            max: 4 * 3600
        }
    );
}

#[test]
fn a_duration_exactly_at_the_policy_maximum_is_accepted() {
    let mut s = spec();
    s.expires_at = s.starts_at + 4 * 3600;
    assert!(SessionGrant::request(&s, &policy(), START).is_ok());
}

#[test]
fn a_start_in_the_past_is_refused() {
    let mut s = spec();
    s.starts_at = START - 5;
    s.expires_at = END;
    let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
    assert!(matches!(err, SessionError::EndBeforeStart { .. }));
}

#[test]
fn an_allread_scope_with_a_ceiling_above_t0_is_refused_at_request_time() {
    for tier in [Tier::T1, Tier::T2, Tier::T3] {
        let mut s = spec();
        s.tier_ceiling = tier;
        s.scope = Scope {
            doctypes: DocTypeScope::AllRead,
            actions: ActionScope::Any,
        };
        let err = SessionGrant::request(&s, &policy(), START).unwrap_err();
        assert!(
            matches!(err, SessionError::ApprovalPolicy(_)),
            "tier {tier:?}"
        );
        assert!(err.to_string().contains("AllRead"));
    }
}

#[test]
fn an_allread_scope_at_t0_is_accepted() {
    let mut s = spec();
    s.tier_ceiling = Tier::T0;
    s.scope = Scope {
        doctypes: DocTypeScope::AllRead,
        actions: ActionScope::Any,
    };
    assert!(SessionGrant::request(&s, &policy(), START).is_ok());
}

#[test]
fn the_request_canonicalises_the_scope_lists() {
    let mut s = spec();
    s.scope = Scope {
        doctypes: DocTypeScope::Named(vec![
            " Payment Entry ".to_owned(),
            "Sales Invoice".to_owned(),
            "Sales Invoice".to_owned(),
            " ".to_owned(),
        ]),
        actions: ActionScope::Only(vec![
            "read".to_owned(),
            " update ".to_owned(),
            "read".to_owned(),
        ]),
    };
    let pending = SessionGrant::request(&s, &policy(), START).unwrap();
    let g = pending.grant();
    // Sorted, deduplicated, trimmed: the whole list must be non-blank, so a list of
    // only blanks would have been refused above.
    assert_eq!(
        g.scope.doctypes,
        DocTypeScope::Named(vec!["Payment Entry".to_owned(), "Sales Invoice".to_owned()])
    );
    assert_eq!(
        g.scope.actions,
        ActionScope::Only(vec!["read".to_owned(), "update".to_owned()])
    );
}

#[test]
fn a_subject_equal_to_the_approver_is_refused() {
    let pending = SessionGrant::request(&spec(), &policy(), START).unwrap();
    let err = pending
        .approve(
            &spot_support_session::Approver::new("agent-support-7", true),
            START,
            &policy(),
        )
        .unwrap_err();
    assert!(matches!(err, SessionError::ApprovalPolicy(_)));
}

#[test]
fn the_policy_defaults_are_the_shipped_ones() {
    let p = SessionPolicy::default();
    assert_eq!(p.max_duration_secs, 4 * 3600);
    assert_eq!(p.max_extensions, 2);
    assert!(p.require_distinct_approver);
    assert_eq!(p.max_tier_without_owner_approval, Tier::T1);
}

#[test]
fn a_spec_for_a_person_subject_round_trips_the_kind() {
    let mut s = spec();
    s.subject = Subject::new(SubjectKind::Person, "claude-user-42");
    let pending = SessionGrant::request(&s, &policy(), START).unwrap();
    assert_eq!(pending.grant().subject.kind, SubjectKind::Person);
}
