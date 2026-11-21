//! `Audience` parsing from outside the crate (ADR-083 test B1 groundwork).

#![allow(clippy::unwrap_used)]

use spot_core::Audience;
use std::collections::BTreeSet;

#[test]
fn the_three_names_round_trip() {
    for (audience, name) in [
        (Audience::Staff, "staff"),
        (Audience::Partner, "partner"),
        (Audience::Consumer, "consumer"),
    ] {
        assert_eq!(audience.as_str(), name);
        assert_eq!(name.parse::<Audience>(), Ok(audience));
        assert_eq!(format!("{audience}"), name);
    }
}

#[test]
fn anything_else_is_refused() {
    for text in ["Staff", " staff", "staff ", "", "admin", "partner\0"] {
        assert!(text.parse::<Audience>().is_err(), "{text:?}");
    }
}

#[test]
fn a_one_mebibyte_string_is_refused_without_echo() {
    let big = "a".repeat(1024 * 1024);
    let error = big.parse::<Audience>().unwrap_err();
    assert!(error.to_string().len() < 200);
}

#[test]
fn audiences_are_copy_ordered_and_hashable() {
    let set: BTreeSet<Audience> = Audience::ALL.into_iter().collect();
    assert_eq!(set.len(), 3);
    let a = Audience::Staff;
    let b = a;
    assert_eq!(a, b);
}
