//! Anonymisation and deletion.
#![allow(missing_docs)]

mod common;

use common::{admin_actor, at, id, internal, user};
use spot_iam_directory::{
    ANONYMISED_DISPLAY_NAME, UserHistory, UserType, anonymise, check_delete, codes, is_anonymised,
};

#[test]
fn personal_fields_are_overwritten_and_the_rest_is_kept() {
    let mut u = user(UserType::Portal);
    u.party_link = Some(id());
    u.manager = Some(id());
    let off = u
        .disable(&admin_actor(), "erasure request", at(50))
        .unwrap_or_else(|e| panic!("{e}"))
        .new_state;
    let a = anonymise(&off).into_user();
    assert_eq!(a.id, u.id);
    assert_eq!(a.user_type, UserType::Portal);
    assert_eq!(a.created_at, u.created_at);
    assert_eq!(a.deactivated_at, Some(at(50)));
    assert_eq!(a.display_name, ANONYMISED_DISPLAY_NAME);
    assert!(a.sign_in_name.starts_with("anonymised-"));
    assert!(!a.sign_in_name.contains("lovelace"), "{}", a.sign_in_name);
    assert_eq!((a.party_link, a.manager), (None, None));
    assert!(!a.enabled);
    assert!(is_anonymised(&a));
    assert!(!is_anonymised(&u));
    // The result is a valid record, and its name is unique per user.
    assert!(a.validate().is_ok());
    assert_ne!(anonymise(&internal()).user().sign_in_name, a.sign_in_name);
}

#[test]
fn anonymising_an_enabled_user_disables_them() {
    let a = anonymise(&internal());
    assert!(!a.user().enabled);
    assert!(a.user().validate().is_ok());
}

#[test]
fn anonymisation_is_idempotent_and_never_changes_the_id() {
    let u = internal();
    let once = anonymise(&u);
    let twice = anonymise(once.user());
    assert_eq!(once, twice);
    assert_eq!(once.user().id, u.id);
}

#[test]
fn an_anonymised_user_cannot_be_enabled_again() {
    let a = anonymise(&internal()).into_user();
    let e = a
        .enable(&admin_actor(), "oops", at(5))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::ANONYMISED]);
}

#[test]
fn only_a_user_who_never_acted_may_be_deleted() {
    let u = internal();
    assert!(check_delete(&u, UserHistory { has_acted: false }).is_ok());
    let e = check_delete(&u, UserHistory { has_acted: true })
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::HAS_ACTED]);
}
