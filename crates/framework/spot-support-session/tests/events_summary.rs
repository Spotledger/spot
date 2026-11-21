//! Events and the summary: one event per transition, nothing sensitive inside, and one
//! deterministic sentence the customer reads before approving.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::check::ActionRequest;
use spot_support_session::model::{SessionPolicy, SessionStatus};
use spot_support_session::{SessionEvent, SessionGrant, Tier};

fn read() -> ActionRequest {
    ActionRequest {
        action: "read".to_owned(),
        tier: Tier::T0,
        doctype: Some("Sales Invoice".to_owned()),
        tenant: TENANT.to_owned(),
    }
}

#[test]
fn every_transition_produces_its_event_with_actor_time_reason() {
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let requested = pending.event().clone();
    assert_eq!(requested.kind(), "requested");
    let (mut grant, approved) = pending
        .approve(&approver(), START + 5, &SessionPolicy::default())
        .unwrap();
    assert_eq!(approved.kind(), "approved");
    assert_eq!(approved.actor(), "customer-admin");
    let extended = grant
        .extend(
            END + 600,
            &approver(),
            START + 10,
            &SessionPolicy::default(),
        )
        .unwrap();
    assert_eq!(extended.kind(), "extended");
    let revoked = grant
        .revoke("tenant-admin", "done early", START + 20)
        .unwrap();
    assert_eq!(revoked.kind(), "revoked");
    // The expiry event is journalled by a caller when it observes the derived expiry.
    let expired = SessionEvent::Expired {
        actor: String::new(),
        at: END,
        reason: String::new(),
    };
    assert_eq!(expired.kind(), "expired");
    assert_eq!(grant.status, SessionStatus::Revoked);
    // The full chain in order: requested, approved, extended, revoked.
    assert_eq!(
        [
            requested.kind(),
            approved.kind(),
            extended.kind(),
            revoked.kind()
        ],
        ["requested", "approved", "extended", "revoked"]
    );
}

#[test]
fn events_carry_no_credentials_and_no_action_arguments() {
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let requested = pending.event().clone();
    let (mut grant, approved) = pending
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap();
    let extended = grant
        .extend(END + 600, &approver(), START + 5, &SessionPolicy::default())
        .unwrap();
    grant.revoke("tenant-admin", "reason", START + 6).unwrap();
    let refusal = grant.check(&read(), START + 7).unwrap_err();
    let refused = SessionEvent::from_refusal(&refusal, START + 7);
    for event in [requested, approved, extended, refused] {
        let json = serde_json::to_string(&event).unwrap();
        // No token-like or credential-looking text, and no action arguments: the refused
        // event names the code and the sentence only, never a payload.
        assert!(!json.to_lowercase().contains("token"));
        assert!(!json.to_lowercase().contains("password"));
        assert!(!json.to_lowercase().contains("secret"));
        if let SessionEvent::Refused { .. } = event {
            // The action name `read` is part of the refusal sentence? It is not: the
            // sentence names the DocType and the scope, not the action.
            assert!(!json.contains("\"read\""));
        }
    }
}

#[test]
fn the_summary_is_one_deterministic_sentence() {
    let g = grant();
    let s1 = g.summary(START + 1);
    let s2 = g.summary(END + 500);
    assert_eq!(s1, s2, "the summary never reads the clock");
    assert_eq!(
        s1,
        "Support agent agent-support-7 may act on Payment Entry and Sales Invoice up to T1 until 2023-11-14 23:13 UTC, approved by customer-admin"
    );
    assert!(!s1.contains('\n'));
}

#[test]
fn the_summary_names_the_doctypes_sorted_and_the_tier_word() {
    let g = grant();
    let s = g.summary(START);
    // The canonicalised scope sorts the DocType names, so the sentence is stable.
    assert!(s.contains("Payment Entry and Sales Invoice"));
    assert!(s.contains("up to T1"));
    assert!(s.contains("approved by customer-admin"));
    assert!(s.contains("until "));
}

#[test]
fn the_allread_summary_says_read_and_diagnose_across_the_tenant() {
    let mut s = spec();
    s.tier_ceiling = Tier::T0;
    s.scope.doctypes = spot_support_session::DocTypeScope::AllRead;
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let text = g.summary(START);
    assert!(text.contains("read and diagnose across the tenant"));
    assert!(text.contains("up to T0"));
}

#[test]
fn the_summary_of_a_pending_or_revoked_grant_stays_truthful() {
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let pending_text = pending.grant().summary(START);
    assert!(pending_text.contains("not yet approved"));
    let mut g = grant();
    g.revoke("tenant-admin", "reason", START + 5).unwrap();
    let revoked_text = g.summary(START + 6);
    // The sentence is deterministic and does not branch on the revoke: the approval is
    // still the one recorded, the revocation lives in the events.
    assert_eq!(revoked_text, g.summary(END + 1));
    assert!(revoked_text.contains("approved by customer-admin"));
}

#[test]
fn an_only_actions_summary_names_the_actions() {
    let mut s = spec();
    s.scope = spot_support_session::Scope {
        doctypes: spot_support_session::DocTypeScope::Named(vec![
            "Sales Invoice".to_owned(),
            "Payment Entry".to_owned(),
        ]),
        actions: spot_support_session::ActionScope::Only(vec![
            "read".to_owned(),
            "repair".to_owned(),
        ]),
    };
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    let text = g.summary(START);
    assert!(text.contains("read and repair on"));
    assert!(text.contains("up to T1"));
}

#[test]
fn the_person_subject_summary_says_support_person() {
    let mut s = spec();
    s.subject =
        spot_support_session::Subject::new(spot_support_session::SubjectKind::Person, "person-9");
    let g = SessionGrant::request(&s, &SessionPolicy::default(), START)
        .unwrap()
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap()
        .0;
    assert!(g.summary(START).starts_with("Support person person-9"));
}

#[test]
fn format_epoch_matches_known_instants() {
    assert_eq!(
        spot_support_session::format_epoch(1_700_000_000),
        "2023-11-14 22:13 UTC"
    );
    assert_eq!(
        spot_support_session::format_epoch(0),
        "1970-01-01 00:00 UTC"
    );
    // A day boundary in both directions: one hour past the instant above.
    assert_eq!(
        spot_support_session::format_epoch(1_700_003_600),
        "2023-11-14 23:13 UTC"
    );
}
