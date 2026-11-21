//! The records: validation, serde, linked identities.
#![allow(missing_docs)]

mod common;

use common::{at, id, internal, user};
use spot_iam_directory::{
    Invitation, LinkedIdentity, User, UserType, check_identity_unique, codes,
};

fn codes_of(u: &User) -> Vec<&'static str> {
    u.validate().err().map(|e| e.codes()).unwrap_or_default()
}

#[test]
fn a_good_user_validates() {
    assert!(internal().validate().is_ok());
    for name in [
        "ada",
        "ada.lovelace",
        "a_b-c.d2",
        "ada@example.com",
        "A.B+tag@mail.example.org",
    ] {
        let mut u = internal();
        u.sign_in_name = name.into();
        assert!(u.validate().is_ok(), "`{name}`");
    }
}

#[test]
fn bad_sign_in_names_are_refused() {
    for name in [
        "",
        "a",
        " ada",
        "ada ",
        "Ada",
        "ada lovelace",
        "ada@",
        "@example.com",
        "ada@example",
        "ada@@example.com",
        &"x".repeat(65),
        &format!("{}@example.com", "x".repeat(250)),
    ] {
        let mut u = internal();
        u.sign_in_name = name.into();
        assert_eq!(codes_of(&u), [codes::INVALID_SIGN_IN_NAME], "`{name}`");
    }
}

#[test]
fn every_problem_is_reported_at_once() {
    let mut u = user(UserType::Internal);
    u.sign_in_name = String::new();
    u.display_name = "  ".into();
    u.party_link = Some(id());
    u.manager = Some(u.id);
    u.enabled = true;
    u.deactivated_at = Some(at(5));
    assert_eq!(
        codes_of(&u),
        [
            codes::INVALID_SIGN_IN_NAME,
            codes::INVALID_DISPLAY_NAME,
            codes::PARTY_LINK,
            codes::MANAGER_SELF,
            codes::INVALID_STATE,
        ]
    );
    // Each problem names its field.
    let errors = u.validate().err().unwrap_or_else(|| panic!("must fail"));
    let fields: Vec<_> = errors.errors().iter().map(|e| e.field).collect();
    assert_eq!(
        fields,
        [
            Some("sign_in_name"),
            Some("display_name"),
            Some("party_link"),
            Some("manager"),
            Some("deactivated_at"),
        ]
    );
}

#[test]
fn only_a_portal_user_has_a_party_link() {
    let mut portal = user(UserType::Portal);
    portal.party_link = Some(id());
    assert!(portal.validate().is_ok());
    for ty in [UserType::Internal, UserType::Service, UserType::Agent] {
        let mut u = user(ty);
        u.party_link = Some(id());
        assert_eq!(codes_of(&u), [codes::PARTY_LINK], "{ty:?}");
    }
}

#[test]
fn a_disabled_user_must_not_predate_their_creation() {
    let mut u = internal();
    u.enabled = false;
    u.deactivated_at = Some(at(-1));
    assert_eq!(codes_of(&u), [codes::INVALID_STATE]);
    u.deactivated_at = None;
    assert!(u.validate().is_ok());
}

#[test]
fn a_user_round_trips_through_toml_and_refuses_unknown_keys() {
    let mut u = internal();
    u.manager = Some(id());
    let text = toml::to_string(&u).unwrap_or_else(|e| panic!("{e}"));
    assert!(text.contains("user_type = \"internal\""), "{text}");
    let back: User = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, u);
    let with_extra = format!("{text}role = \"admin\"\n");
    assert!(toml::from_str::<User>(&with_extra).is_err());
}

#[test]
fn a_record_id_must_be_in_canonical_form() {
    let u = internal();
    let text = toml::to_string(&u).unwrap_or_else(|e| panic!("{e}"));
    let upper = text.replace(&u.id.to_string(), &u.id.to_string().to_uppercase());
    assert!(toml::from_str::<User>(&upper).is_err());
    let short = text.replace(&u.id.to_string(), "not-an-id");
    assert!(toml::from_str::<User>(&short).is_err());
    // Not version 7.
    let v4 = text.replace(&u.id.to_string(), "67e55044-10b1-426f-9247-bb680e5fe0c8");
    assert!(toml::from_str::<User>(&v4).is_err());
}

#[test]
fn timestamps_are_written_in_utc() {
    let u = internal();
    let text = toml::to_string(&u).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        text.contains("created_at = \"2026-10-08T00:00:00Z\""),
        "{text}"
    );
}

#[test]
fn an_invitation_round_trips() {
    let inv = Invitation::new(id(), "a@b.example", UserType::Portal, at(3600));
    let text = toml::to_string(&inv).unwrap_or_else(|e| panic!("{e}"));
    assert!(text.contains("state = \"pending\""), "{text}");
    let back: Invitation = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, inv);
}

fn link(user: spot_storage_contract::RecordId, issuer: &str, subject: &str) -> LinkedIdentity {
    LinkedIdentity {
        user,
        issuer: issuer.into(),
        subject: subject.into(),
    }
}

#[test]
fn an_identity_is_unique_per_issuer_and_subject() {
    let (a, b) = (id(), id());
    let existing = vec![
        link(a, "https://idp.example", "sub-1"),
        link(a, "https://idp.example", "sub-2"),
        link(a, "https://other.example", "sub-1"),
    ];
    // A user has several identities.
    assert!(check_identity_unique(&existing, &link(a, "https://idp.example", "sub-3")).is_ok());
    // The same user again is harmless.
    assert!(check_identity_unique(&existing, &link(a, "https://idp.example", "sub-1")).is_ok());
    // Another user may use the subject of another issuer, never the same pair.
    assert!(check_identity_unique(&existing, &link(b, "https://third.example", "sub-1")).is_ok());
    let e = check_identity_unique(&existing, &link(b, "https://idp.example", "sub-1"))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::IDENTITY_TAKEN]);
}

#[test]
fn an_identity_needs_an_issuer_and_a_subject() {
    let e = link(id(), "", "")
        .validate()
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(
        e.codes(),
        [codes::INVALID_IDENTITY, codes::INVALID_IDENTITY]
    );
    assert!(
        link(id(), "https://idp.example", &"s".repeat(3000))
            .validate()
            .is_err()
    );
    assert!(
        link(id(), "https://idp.example", "a\nb")
            .validate()
            .is_err()
    );
    assert!(link(id(), "https://idp.example", "abc").validate().is_ok());
}
