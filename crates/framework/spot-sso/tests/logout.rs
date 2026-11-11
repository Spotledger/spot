//! Resolving a logout target to linked identities: only the identity of **exactly** the
//! provider that signed the logout matches, and an unknown person is acknowledged with an
//! empty result rather than an error.

use spot_iam_directory::LinkedIdentity;
use spot_sso::{LogoutTarget, ProviderId, resolve_target};
use spot_storage_contract::RecordId;

const ISSUER: &str = "https://idp.example.com";
const OTHER_ISSUER: &str = "https://other-idp.example.com";

fn identity(user: RecordId, issuer: &str, subject: &str) -> LinkedIdentity {
    LinkedIdentity {
        user,
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
    }
}

fn target(issuer: &str, subject: Option<&str>) -> LogoutTarget {
    LogoutTarget::for_oidc(
        ProviderId::new("acme"),
        issuer,
        subject.map(str::to_owned),
        None,
    )
}

#[test]
fn the_identity_of_exactly_that_provider_is_returned() {
    let user = RecordId::new_v7();
    let linked = vec![identity(user, ISSUER, "subject-1")];
    let resolved = resolve_target(&target(ISSUER, Some("subject-1")), &linked);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].user, user);
    assert_eq!(resolved[0].issuer, ISSUER);
    assert_eq!(resolved[0].subject, "subject-1");
}

#[test]
fn the_same_subject_at_another_provider_is_a_different_person() {
    let user = RecordId::new_v7();
    let linked = vec![identity(user, ISSUER, "subject-1")];
    assert!(resolve_target(&target(OTHER_ISSUER, Some("subject-1")), &linked).is_empty());
}

#[test]
fn an_unknown_subject_is_acknowledged_with_nothing() {
    let user = RecordId::new_v7();
    let linked = vec![identity(user, ISSUER, "subject-1")];
    assert!(resolve_target(&target(ISSUER, Some("someone-else")), &linked).is_empty());
    assert!(resolve_target(&target(ISSUER, None), &linked).is_empty());
}

#[test]
fn an_empty_subject_matches_nothing() {
    let user = RecordId::new_v7();
    let linked = vec![identity(user, ISSUER, "")];
    assert!(resolve_target(&target(ISSUER, Some("")), &linked).is_empty());
}

#[test]
fn one_user_is_returned_once_even_with_duplicate_links() {
    let user = RecordId::new_v7();
    let linked = vec![
        identity(user, ISSUER, "subject-1"),
        identity(user, ISSUER, "subject-1"),
    ];
    let resolved = resolve_target(&target(ISSUER, Some("subject-1")), &linked);
    assert_eq!(resolved.len(), 1);
}

#[test]
fn every_distinct_user_of_the_subject_is_returned() {
    let first = RecordId::new_v7();
    let second = RecordId::new_v7();
    let linked = vec![
        identity(first, ISSUER, "subject-1"),
        identity(second, ISSUER, "subject-1"),
    ];
    let resolved = resolve_target(&target(ISSUER, Some("subject-1")), &linked);
    assert_eq!(resolved.len(), 2);
}
