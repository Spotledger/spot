//! Canonicalisation is a pure function of the string: idempotent, total, and blind to case and
//! punctuation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;

use spot_consumer_admission::{Destination, DestinationKind, canonicalise};

proptest! {
    /// Arbitrary bytes in either shape: an answer or `bad_destination`, never a panic, and a
    /// canonical form that canonicalises to itself.
    #[test]
    fn canonicalisation_is_total_and_idempotent(raw in ".{0,80}", phone in any::<bool>()) {
        let destination = if phone {
            Destination::Phone(&raw)
        } else {
            Destination::Email(&raw)
        };
        for strip in [true, false] {
            if let Ok(canonical) = canonicalise(destination, strip) {
                let again = match canonical.kind() {
                    DestinationKind::Email => Destination::Email(canonical.as_str()),
                    DestinationKind::Phone => Destination::Phone(canonical.as_str()),
                };
                let twice = canonicalise(again, strip).expect("a canonical form is canonical");
                prop_assert_eq!(twice.as_str(), canonical.as_str());
                prop_assert!(canonical.as_str().is_ascii());
            }
        }
    }

    /// Case never makes a new destination, whichever way the local part and domain are spelled.
    #[test]
    fn case_never_makes_a_new_destination(local in "[a-z]{1,20}", domain in "[a-z]{1,12}\\.[a-z]{2,6}") {
        let lowered = format!("{local}@{domain}");
        let shouted = format!("{}@{}", local.to_uppercase(), domain.to_uppercase());
        let one = canonicalise(Destination::Email(&lowered), true).expect("accepted");
        let two = canonicalise(Destination::Email(&shouted), true).expect("accepted");
        prop_assert_eq!(one.as_str(), two.as_str());
    }

    /// A plus tag is the same mailbox when the policy strips it, and a different string when it
    /// does not.
    #[test]
    fn plus_tags_are_the_same_mailbox_only_when_stripped(
        local in "[a-z][a-z0-9]{0,15}",
        tag in "[a-z0-9]{1,10}",
    ) {
        let plain = format!("{local}@example.com");
        let tagged = format!("{local}+{tag}@example.com");
        let stripped = canonicalise(Destination::Email(&tagged), true).expect("accepted");
        let kept = canonicalise(Destination::Email(&tagged), false).expect("accepted");
        let base = canonicalise(Destination::Email(&plain), true).expect("accepted");
        prop_assert_eq!(stripped.as_str(), base.as_str());
        prop_assert_ne!(kept.as_str(), base.as_str());
    }

    /// Phone punctuation never makes a new number: the canonical form is the digits alone.
    #[test]
    fn phone_punctuation_never_makes_a_new_number(digits in "[1-9][0-9]{7,14}") {
        let plain = format!("+{digits}");
        let spaced = format!(
            "+{} {} {}",
            &digits[..2],
            &digits[2..4],
            &digits[4..]
        );
        let bracketed = format!("+{} ({}) {}-{}", &digits[..1], &digits[1..4], &digits[4..6], &digits[6..]);
        let canonical = canonicalise(Destination::Phone(&plain), true).expect("accepted");
        prop_assert_eq!(canonical.as_str(), plain.as_str());
        for spelling in [spaced, bracketed] {
            let other = canonicalise(Destination::Phone(&spelling), true).expect("accepted");
            prop_assert_eq!(other.as_str(), canonical.as_str());
        }
    }

    /// A phone without a country code is refused, never guessed at.
    #[test]
    fn phone_without_a_plus_is_refused(digits in "[0-9]{8,15}") {
        prop_assert!(canonicalise(Destination::Phone(&digits), true).is_err());
    }
}
