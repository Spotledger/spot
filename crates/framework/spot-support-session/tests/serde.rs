//! The serde story: a grant with extension history round trips through JSON and TOML,
//! and the wire shape is the stable contract (WP-CTL-07 stores these).

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::model::{ActionScope, DocTypeScope, Extension, Scope, SessionPolicy};
use spot_support_session::{SessionGrant, SessionStatus, Tier};

fn grant_with_history() -> SessionGrant {
    let mut g = grant();
    let p = SessionPolicy::default();
    g.extend(END + 1800, &approver(), START + 60, &p).unwrap();
    g
}

#[test]
fn a_grant_with_history_round_trips_through_json() {
    let g = grant_with_history();
    let json = serde_json::to_string(&g).unwrap();
    let back: SessionGrant = serde_json::from_str(&json).unwrap();
    assert_eq!(back, g);
    assert_eq!(back.extensions().len(), 1);
    assert_eq!(back.extensions()[0].extended_to, END + 1800);
    assert_eq!(back.status, SessionStatus::Active);
}

#[test]
fn a_grant_with_history_round_trips_through_toml() {
    let g = grant_with_history();
    let text = toml::to_string(&g).unwrap();
    let back: SessionGrant = toml::from_str(&text).unwrap();
    assert_eq!(back, g);
}

#[test]
fn a_revoked_grant_round_trips_its_revocation() {
    let mut g = grant();
    g.revoke("tenant-admin", "the ticket is resolved", START + 120)
        .unwrap();
    let json = serde_json::to_string(&g).unwrap();
    let back: SessionGrant = serde_json::from_str(&json).unwrap();
    assert_eq!(back, g);
    let r = back.revocation().unwrap();
    assert_eq!(r.by, "tenant-admin");
    assert_eq!(r.reason, "the ticket is resolved");
}

#[test]
fn a_pending_grant_round_trips() {
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let json = serde_json::to_string(&pending).unwrap();
    let back: spot_support_session::PendingGrant = serde_json::from_str(&json).unwrap();
    assert_eq!(back, pending);
    assert_eq!(back.grant().status, SessionStatus::Pending);
}

#[test]
fn an_empty_history_and_no_revocation_are_omitted_from_the_wire() {
    let g = grant();
    let json = serde_json::to_string(&g).unwrap();
    assert!(!json.contains("extensions"), "{json}");
    assert!(!json.contains("revoked"), "{json}");
}

#[test]
fn the_wire_uses_snake_case_names_and_the_json_round_trips_an_allread() {
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
    let json = serde_json::to_string(&g).unwrap();
    assert!(json.contains("\"all_read\""));
    assert!(json.contains("\"tier_ceiling\""));
    let back: SessionGrant = serde_json::from_str(&json).unwrap();
    assert_eq!(back, g);
}

#[test]
fn an_unknown_field_in_the_wire_is_an_error_not_ignored() {
    let g = grant();
    let mut json = serde_json::to_string(&g).unwrap();
    // Splice a key the model does not know, so a typo is caught at load.
    let at = json.rfind('}').unwrap();
    json.insert_str(at, ",\"permlevel\":1");
    let back: Result<SessionGrant, _> = serde_json::from_str(&json);
    assert!(back.is_err());
}

#[test]
fn an_extension_record_round_trips_alone() {
    let e = Extension {
        approver: "customer-admin".to_owned(),
        at: START,
        extended_to: END,
    };
    let json = serde_json::to_string(&e).unwrap();
    let back: Extension = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);
}

#[test]
fn two_grants_equal_after_a_load_write_load_cycle_compare_equal() {
    let a = grant_with_history();
    let json = serde_json::to_string(&a).unwrap();
    let b: SessionGrant = serde_json::from_str(&json).unwrap();
    let json2 = serde_json::to_string(&b).unwrap();
    assert_eq!(json, json2);
    assert_eq!(a, b);
}
