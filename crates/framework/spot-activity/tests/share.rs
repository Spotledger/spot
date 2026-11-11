//! Shares: union, replacement, expiry including the exact boundary (WP-DOC-07.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_activity::error::ActivityError;
use spot_activity::share::{Share, ShareRight, ShareRights, effective_share_rights};

fn grant(
    grantee: &str,
    rights: &[ShareRight],
    granted_at: i64,
    expires_secs: Option<i64>,
) -> Share {
    Share::new(
        uuid7(3, 1),
        grantee,
        rights.iter().copied(),
        "owner",
        at(granted_at),
        expires_secs.map(at),
    )
    .unwrap()
}

#[test]
fn rights_union_across_principals() {
    // Replacement is per (doc, grantee) pair — a later grant to the same grantee replaces the
    // earlier (the next test pins that) — so union happens across principals: the grants held
    // through a user's roles union with each other and with the user's own grant.
    let doc = uuid7(3, 1);
    let shares = vec![
        grant("purchasing", &[ShareRight::Read], 0, None),
        grant("sales", &[ShareRight::Write], 5, None),
    ];
    let rights = effective_share_rights(&shares, doc, "bob", &["purchasing", "sales"], at(10));
    assert!(rights.contains(ShareRight::Read));
    assert!(rights.contains(ShareRight::Write));
    assert!(!rights.contains(ShareRight::Submit));
}

#[test]
fn a_later_grant_for_the_same_grantee_replaces_the_earlier() {
    // Careful reading of the brief: `effective_share_rights` unions the *unexpired grants*,
    // "a later grant for the same grantee replaces the earlier". The union test above shows
    // two live grants of different rights union; this test pins the replacement rule: the
    // later grant replaces the earlier *for the same grantee pair* when both carry the same
    // right set shape — replacement is per grantee, so the later grant's full set wins and the
    // earlier one's extra rights do not survive.
    let doc = uuid7(3, 1);
    let shares = vec![
        grant("bob", &[ShareRight::Read, ShareRight::Write], 0, None),
        grant("bob", &[ShareRight::Read], 5, None),
    ];
    let rights = effective_share_rights(&shares, doc, "bob", &[], at(10));
    // The later grant (Read only) replaced the earlier (Read+Write).
    assert!(rights.contains(ShareRight::Read));
    assert!(!rights.contains(ShareRight::Write), "{rights:?}");
}

#[test]
fn grants_to_other_users_or_docs_do_not_count() {
    let doc = uuid7(3, 1);
    let shares = vec![
        grant("carol", &[ShareRight::Write], 0, None),
        Share::new(
            uuid7(3, 2), // another doc
            "bob",
            [ShareRight::Write],
            "owner",
            at(0),
            None,
        )
        .unwrap(),
    ];
    let rights = effective_share_rights(&shares, doc, "bob", &[], at(10));
    assert!(rights.is_empty());
}

#[test]
fn an_expired_grant_is_ignored() {
    let doc = uuid7(3, 1);
    let shares = vec![grant("bob", &[ShareRight::Read], 0, Some(5))];
    assert!(effective_share_rights(&shares, doc, "bob", &[], at(6)).is_empty());
    // Still live just before the boundary.
    assert!(effective_share_rights(&shares, doc, "bob", &[], at(4)).contains(ShareRight::Read));
}

#[test]
fn expiry_on_the_exact_boundary_is_expired() {
    // `now == expires_at` is expired (`now >= expires_at`), tested at the exact instant.
    let doc = uuid7(3, 1);
    let shares = vec![grant("bob", &[ShareRight::Read], 0, Some(5))];
    assert!(effective_share_rights(&shares, doc, "bob", &[], at(5)).is_empty());
    let rights = effective_share_rights(&shares, doc, "bob", &[], at(4));
    assert!(rights.contains(ShareRight::Read));
}

#[test]
fn a_grant_without_expiry_never_expires() {
    let doc = uuid7(3, 1);
    let shares = vec![grant("bob", &[ShareRight::Read], 0, None)];
    assert!(
        effective_share_rights(&shares, doc, "bob", &[], at(1_000_000)).contains(ShareRight::Read)
    );
}

#[test]
fn roles_hold_grants_too() {
    let doc = uuid7(3, 1);
    let shares = vec![grant("purchasing", &[ShareRight::Submit], 0, None)];
    let rights = effective_share_rights(&shares, doc, "bob", &["purchasing"], at(10));
    assert!(rights.contains(ShareRight::Submit));
    // The user's own grant and the role's union.
    let shares = vec![
        grant("bob", &[ShareRight::Read], 0, None),
        grant("purchasing", &[ShareRight::Submit], 0, None),
    ];
    let rights = effective_share_rights(&shares, doc, "bob", &["purchasing"], at(10));
    assert!(rights.contains(ShareRight::Read) && rights.contains(ShareRight::Submit));
}

#[test]
fn an_empty_live_set_is_no_rights() {
    let doc = uuid7(3, 1);
    assert!(effective_share_rights(&[], doc, "bob", &[], at(0)).is_empty());
}

#[test]
fn rights_serialise_in_canonical_order_and_round_trip() {
    let rights = ShareRights::new([ShareRight::Submit, ShareRight::Read]).unwrap();
    let json = serde_json::to_string(&rights).unwrap();
    assert_eq!(json, r#"["read","submit"]"#);
    let back: ShareRights = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rights);
}

#[test]
fn an_empty_right_set_is_refused() {
    let e = ShareRights::new(Vec::<ShareRight>::new()).unwrap_err();
    assert_eq!(e.code(), "activity.path_invalid");
    match &e {
        ActivityError::PathInvalid(_) => {}
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn share_serde_round_trip_and_names() {
    let s = grant("bob", &[ShareRight::Read, ShareRight::Write], 0, Some(60));
    let json = serde_json::to_string(&s).unwrap();
    let back: Share = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    for key in ["doc", "grantee", "rights", "granted_by", "at", "expires_at"] {
        assert!(parsed.get(key).is_some(), "missing {key} in {json}");
    }
}

#[test]
fn a_share_without_expiry_omits_the_field() {
    let s = grant("bob", &[ShareRight::Read], 0, None);
    let parsed: serde_json::Value = serde_json::to_value(&s).unwrap();
    assert!(parsed.get("expires_at").is_none());
}
