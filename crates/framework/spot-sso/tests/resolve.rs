//! From a validated identity to a sign-in decision.

mod support;

use spot_iam_directory::{LinkedIdentity, UserType};
use spot_sso::{
    AuthMethod, JitPolicy, JitRule, ProviderId, Resolution, SignInMode, SignInPolicy, SsoIdentity,
    link_identity, resolve,
};

use support::*;

/// An identity from the fake provider.
fn identity(subject: &str, email: &str, verified: bool) -> SsoIdentity {
    SsoIdentity {
        provider: ProviderId::new("acme"),
        issuer: ISSUER.to_owned(),
        subject: subject.to_owned(),
        email: Some(email.to_owned()),
        email_verified: verified,
        display_name: Some("Ada Lovelace".to_owned()),
        groups: vec!["engineering".to_owned()],
        acr: Some("urn:mace:incommon:iap:silver".to_owned()),
        amr: vec!["pwd".to_owned()],
        auth_time: Some(1_700_000_000),
        raw_claims_hash: "hash".to_owned(),
    }
}

/// A policy that allows SSO with the fake provider.
fn sso_policy() -> SignInPolicy {
    SignInPolicy {
        mode: SignInMode::SsoOptional,
        providers: vec![ProviderId::new("acme")],
        ..SignInPolicy::local()
    }
}

#[test]
fn a_known_identity_signs_in_at_sso_strength() {
    let mut dir = FakeDirectory::new();
    let id = dir.add(user(new_id(), "ada@example.com"));
    dir.link(ISSUER, "subject-1", id);
    match resolve(
        &identity("subject-1", "ada@example.com", true),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::SignIn { user, method } => {
            assert_eq!(user, id);
            assert_eq!(method, AuthMethod::Sso);
        }
        other => panic!("expected a sign-in, got {other:?}"),
    }
}

#[test]
fn a_disabled_user_is_denied() {
    let mut dir = FakeDirectory::new();
    let mut disabled = user(new_id(), "ada@example.com");
    disabled.enabled = false;
    let id = dir.add(disabled);
    dir.link(ISSUER, "subject-1", id);
    match resolve(
        &identity("subject-1", "ada@example.com", true),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.user_disabled"),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn a_user_without_sso_is_denied() {
    let mut dir = FakeDirectory::new();
    let mut no_sso = user(new_id(), "ada@example.com");
    no_sso.allows_sso = false;
    let id = dir.add(no_sso);
    dir.link(ISSUER, "subject-1", id);
    match resolve(
        &identity("subject-1", "ada@example.com", true),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.sso_not_allowed"),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn an_unknown_identity_with_an_invitation_links() {
    let mut dir = FakeDirectory::new();
    let invitation = new_id();
    dir.invite("new@example.com", invitation);
    match resolve(
        &identity("subject-9", "new@example.com", true),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::LinkViaInvitation {
            invitation: found,
            email,
        } => {
            assert_eq!(found, invitation);
            assert_eq!(email, "new@example.com");
        }
        other => panic!("expected an invitation link, got {other:?}"),
    }
}

#[test]
fn an_email_match_without_proof_requires_confirmation() {
    let mut dir = FakeDirectory::new();
    let id = dir.add(user(new_id(), "ada@example.com"));
    match resolve(
        &identity("subject-new", "ada@example.com", true),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::RequireLocalConfirmation { user } => assert_eq!(user, id),
        other => panic!("expected a confirmation, got {other:?}"),
    }
}

#[test]
fn an_unverified_email_never_links() {
    let mut dir = FakeDirectory::new();
    dir.invite("new@example.com", new_id());
    match resolve(
        &identity("subject-9", "new@example.com", false),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.unknown_identity"),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn a_matching_jit_rule_creates_a_user_with_no_roles() {
    let dir = FakeDirectory::new();
    let policy = SignInPolicy {
        jit: JitPolicy::RuleBased(vec![JitRule {
            email_domain: Some("example.com".to_owned()),
            group: None,
            user_type: UserType::Internal,
        }]),
        ..sso_policy()
    };
    match resolve(
        &identity("subject-9", "new@example.com", true),
        &dir,
        &policy,
        at(0),
    ) {
        Resolution::CreateUser {
            user_type,
            identity,
            audit,
        } => {
            assert_eq!(user_type, UserType::Internal);
            assert_eq!(identity.subject, "subject-9");
            assert_eq!(audit.subject, "subject-9");
            assert_eq!(audit.user_type, UserType::Internal);
        }
        other => panic!("expected a just-in-time user, got {other:?}"),
    }
}

#[test]
fn jit_off_denies_an_unknown_identity() {
    let dir = FakeDirectory::new();
    match resolve(
        &identity("subject-9", "new@example.com", true),
        &dir,
        &sso_policy(),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.unknown_identity"),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn a_local_policy_denies_every_sso_sign_in() {
    let mut dir = FakeDirectory::new();
    let id = dir.add(user(new_id(), "ada@example.com"));
    dir.link(ISSUER, "subject-1", id);
    match resolve(
        &identity("subject-1", "ada@example.com", true),
        &dir,
        &SignInPolicy::local(),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.sso_not_allowed"),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn linking_refuses_an_identity_held_by_another_user() {
    let first = new_id();
    let second = new_id();
    let existing = vec![LinkedIdentity {
        user: first,
        issuer: ISSUER.to_owned(),
        subject: "subject-1".to_owned(),
    }];
    let contested = LinkedIdentity {
        user: second,
        issuer: ISSUER.to_owned(),
        subject: "subject-1".to_owned(),
    };
    let error = err(link_identity(&existing, &contested));
    assert_eq!(error.code(), "sso.identity_other_user");
}

#[test]
fn the_same_subject_at_two_issuers_is_two_identities() {
    let user_id = new_id();
    let existing = vec![LinkedIdentity {
        user: user_id,
        issuer: "https://a.example.com".to_owned(),
        subject: "shared-subject".to_owned(),
    }];
    let other_issuer = LinkedIdentity {
        user: user_id,
        issuer: "https://b.example.com".to_owned(),
        subject: "shared-subject".to_owned(),
    };
    assert!(link_identity(&existing, &other_issuer).is_ok());
}
