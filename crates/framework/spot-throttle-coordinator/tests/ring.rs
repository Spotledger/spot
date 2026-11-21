//! [`SaltRing`]: rotation, retirement and the window, with no coordinator involved.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use spot_coordinator::RateLimit;
use spot_throttle_coordinator::{SaltRing, ThrottlePolicy, ThrottleSalt};
use time::OffsetDateTime;

const SALT_BYTES: [u8; 32] = [7; 32];

fn salt(byte: u8) -> ThrottleSalt {
    ThrottleSalt::new(&[byte; 32]).unwrap()
}

fn limit(capacity: u64, secs: u64) -> RateLimit {
    RateLimit {
        capacity,
        per: Duration::from_secs(secs),
    }
}

/// Account 1m, source 1h, destination 2m, tenant 2h: the longest window is 2h.
fn policy() -> ThrottlePolicy {
    ThrottlePolicy::new(
        limit(3, 60),
        limit(100, 3600),
        limit(3, 120),
        limit(1000, 7200),
    )
    .unwrap()
}

#[test]
fn a_rotation_moves_the_current_salt_to_previous() {
    let first = salt(1);
    let second = salt(2);
    let ring = SaltRing::new(first.clone());
    assert!(!ring.has_previous());
    assert!(ring.current().same_key(&first));

    let rotated = ring.rotate(second.clone());
    assert!(rotated.has_previous());
    assert!(rotated.current().same_key(&second));
    assert!(
        rotated.previous().is_some_and(|p| p.same_key(&first)),
        "the salt the new one replaced is the previous one"
    );
}

#[test]
fn rotating_twice_drops_the_older_previous() {
    let ring = SaltRing::new(salt(1)).rotate(salt(2)).rotate(salt(3));
    assert!(ring.current().same_key(&salt(3)));
    assert!(
        ring.previous().is_some_and(|p| p.same_key(&salt(2))),
        "the ring keeps one previous salt, never a history"
    );
    assert!(!ring.previous().is_some_and(|p| p.same_key(&salt(1))));
}

#[test]
fn retire_previous_drops_it_and_keeps_the_current_salt() {
    let mut ring = SaltRing::with_previous(salt(2), salt(1));
    assert!(ring.has_previous());
    ring.retire_previous();
    assert!(!ring.has_previous());
    assert!(ring.previous().is_none());
    assert!(ring.current().same_key(&salt(2)));
}

#[test]
fn safe_to_retire_is_false_until_one_full_window_of_the_longest_bucket() {
    let rotated_at = OffsetDateTime::UNIX_EPOCH;
    let policy = policy();
    let longest = policy.longest_window();
    assert_eq!(
        longest,
        Duration::from_secs(7200),
        "the tenant window is longest"
    );

    let at = |secs: i64| rotated_at + time::Duration::seconds(secs);
    for before in [
        rotated_at,
        at(1),
        at(3600),
        at(7199),
        rotated_at + time::Duration::seconds(7200) - time::Duration::milliseconds(1),
    ] {
        assert!(
            !SaltRing::safe_to_retire(rotated_at, before, &policy),
            "{before} is inside the window"
        );
    }
    // Exactly one full window: every bucket has refilled, so retiring now forgets nothing.
    assert!(SaltRing::safe_to_retire(rotated_at, at(7200), &policy));
    assert!(SaltRing::safe_to_retire(rotated_at, at(7201), &policy));
    assert!(SaltRing::safe_to_retire(rotated_at, at(86_400), &policy));
}

#[test]
fn safe_to_retire_refuses_when_the_clock_went_backwards() {
    let rotated_at = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1);
    let now = rotated_at - time::Duration::seconds(1);
    assert!(
        !SaltRing::safe_to_retire(rotated_at, now, &policy()),
        "a now before the rotation proves nothing"
    );
}

#[test]
fn remaining_window_counts_down_and_ends_at_the_window() {
    let rotated_at = OffsetDateTime::UNIX_EPOCH;
    let policy = policy();
    let ring = SaltRing::with_previous(salt(2), salt(1));
    assert_eq!(
        ring.remaining_window(rotated_at, rotated_at, &policy),
        Some(Duration::from_secs(7200))
    );
    assert_eq!(
        ring.remaining_window(
            rotated_at,
            rotated_at + time::Duration::seconds(5400),
            &policy
        ),
        Some(Duration::from_secs(1800))
    );
    assert_eq!(
        ring.remaining_window(
            rotated_at,
            rotated_at + time::Duration::seconds(7200),
            &policy
        ),
        None
    );
    // A ring with nothing to retire has nothing to wait for.
    let single = SaltRing::new(salt(2));
    assert_eq!(
        single.remaining_window(rotated_at, rotated_at, &policy),
        None
    );
}

#[test]
fn rotate_checked_refuses_the_salt_already_in_force() {
    let current = salt(1);
    let error = SaltRing::new(current.clone())
        .rotate_checked(
            current,
            OffsetDateTime::UNIX_EPOCH,
            OffsetDateTime::UNIX_EPOCH,
            &policy(),
        )
        .unwrap_err();
    assert_eq!(error.code(), "throttle_coordinator.bad_rotation");
}

#[test]
fn rotate_checked_refuses_to_cut_the_previous_window_short() {
    let rotated_at = OffsetDateTime::UNIX_EPOCH;
    let early = SaltRing::with_previous(salt(2), salt(1)).rotate_checked(
        salt(3),
        rotated_at,
        rotated_at + time::Duration::seconds(60),
        &policy(),
    );
    let error = early.unwrap_err();
    assert_eq!(error.code(), "throttle_coordinator.bad_rotation");
    assert!(
        !error.to_string().contains("spot"),
        "the message names no key and no salt"
    );
}

#[test]
fn rotate_checked_allows_a_rotation_after_the_window_and_the_first_one() {
    let rotated_at = OffsetDateTime::UNIX_EPOCH;
    let ring = SaltRing::with_previous(salt(2), salt(1))
        .rotate_checked(
            salt(3),
            rotated_at,
            rotated_at + time::Duration::seconds(7200),
            &policy(),
        )
        .unwrap();
    assert!(ring.current().same_key(&salt(3)));
    assert!(ring.previous().is_some_and(|p| p.same_key(&salt(2))));

    // The first rotation has no previous salt to preserve, so no window applies to it.
    let fresh = SaltRing::new(salt(1))
        .rotate_checked(salt(2), rotated_at, rotated_at, &policy())
        .unwrap();
    assert!(fresh.has_previous());
}

#[test]
fn the_debug_of_a_ring_never_shows_a_salt() {
    let ring = SaltRing::with_previous(salt(1), ThrottleSalt::new(&SALT_BYTES).unwrap());
    let debug = format!("{ring:?}");
    assert!(debug.contains("ThrottleSalt(<redacted>)"), "{debug}");
    assert!(!debug.contains("07070707"), "{debug}");
    assert!(!debug.contains("01010101"), "{debug}");
    assert!(
        debug.contains("previous: Some"),
        "whether a previous salt exists is an operational fact: {debug}"
    );
    assert_eq!(format!("{:?}", salt(7)), "ThrottleSalt(<redacted>)");
}

#[test]
fn same_key_compares_the_secret_not_the_bytes() {
    assert!(salt(1).same_key(&salt(1)));
    assert!(!salt(1).same_key(&salt(2)));
    // Same prefix, different length: not the same key.
    let short = ThrottleSalt::new(&[1; 16]).unwrap();
    assert!(!short.same_key(&salt(1)));
}
