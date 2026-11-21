//! Property tests (brief §7): `evaluate` monotonic in `now` for a valid licence, and
//! `verify` never panics on arbitrary bytes.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself
//! built.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ed25519_dalek::{SigningKey, VerifyingKey};
use proptest::prelude::*;
use spot_license::envelope::{SignedLicence, TrustedKey, TrustedKeys, verify};
use spot_license::model::{Entitlements, Kind, Licence, PAYLOAD_VERSION, RawLicence};
use spot_license::state::{Status, evaluate};

/// A validated subscription licence; fixtures mutate single fields.
fn subscription() -> Licence {
    Licence::new(RawLicence {
        v: PAYLOAD_VERSION,
        licence_id: "0198c7a2-license-id".to_owned(),
        tenant: Some("acme".to_owned()),
        installation_id: None,
        edition: spot_license::model::Edition::new("business").unwrap_or_else(|e| panic!("{e:?}")),
        ent: Entitlements::default(),
        kind: Kind::Subscription,
        issued_at: 1_770_000_000,
        not_before: 1_770_000_000,
        expires_at: Some(1_772_683_200),
        renew_after: Some(1_772_071_200),
        grace: 604_800,
        serial: 1,
        key_id: "issuer-2026-q3".to_owned(),
    })
    .unwrap_or_else(|error| panic!("{error:?}"))
}

/// The lifecycle order of a status, in crate-doc table order. `evaluate` may stall on
/// an observation (the trial rule) but the *clock-walk* rank never regresses: a state
/// earlier in this order is never returned for a `now` that produced a later one.
fn rank(status: &Status) -> u8 {
    match status {
        Status::NotYetValid => 0,
        Status::Active => 1,
        Status::RenewalDue => 2,
        Status::Grace { .. } | Status::ClockSuspect { .. } => 3,
        Status::Restricted => 4,
        Status::Expired => 4,
    }
}

/// A random `now` on the scale of the fixtures.
fn now_strategy() -> impl Strategy<Value = i64> {
    0i64..3_500_000_000
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The monotonicity property: for a valid subscription licence, walking `now`
    /// forward (or standing still) never moves the lifecycle state backwards.
    #[test]
    fn evaluate_is_monotonic_in_now(
        first in now_strategy(),
        second in now_strategy(),
        last_seen in proptest::option::of(1_600_000_000i64..2_200_000_000),
    ) {
        let licence = subscription();
        let earlier = evaluate(&licence, first.min(second), last_seen);
        let later = evaluate(&licence, first.max(second), last_seen);
        prop_assert!(
            rank(&earlier) <= rank(&later),
            "state went back: {earlier:?} -> {later:?}"
        );
    }

    /// The monotonicity property across a *sequential walk*: one hundred seconds
    /// stepping forward from a random start; the rank at every step is at least the
    /// previous rank.
    #[test]
    fn evaluate_walk_is_never_regressing(
        start in now_strategy(),
        step_size in 1i64..(86_400 * 7),
    ) {
        let licence = subscription();
        let mut previous = rank(&evaluate(&licence, start, None));
        for step in 0..100 {
            let status = evaluate(&licence, start.saturating_add(step_size * step), None);
            let current = rank(&status);
            prop_assert!(current >= previous, "state went back at step {step}");
            previous = current;
        }
    }

    /// Boundary exactness, property form: at `renew_after` exactly, the state is
    /// `RenewalDue` (never `Active` and never past-valid), for every window shape.
    #[test]
    fn renew_after_boundary_is_exact(
        issued in 1_000_000_000i64..1_800_000_000,
        lifetime in 1i64..86_400 * 400,
        renew_window in 0i64..86_400 * 400,
    ) {
        let expires_at = issued.saturating_add(lifetime).min(2_240_000_000);
        let renew_after = issued.saturating_add(renew_window.min(lifetime)).min(expires_at);
        let raw = RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: "0198c7a2-license-id".to_owned(),
            tenant: Some("acme".to_owned()),
            installation_id: None,
            edition: spot_license::model::Edition::new("business")
                .unwrap_or_else(|e| panic!("{e:?}")),
            ent: Entitlements::default(),
            kind: Kind::Subscription,
            issued_at: issued,
            not_before: issued,
            expires_at: Some(expires_at),
            renew_after: Some(renew_after),
            grace: 604_800,
            serial: 1,
            key_id: "issuer-2026-q3".to_owned(),
        };
        let licence = Licence::new(raw.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        prop_assert_eq!(evaluate(&licence, renew_after, None), Status::RenewalDue);
        if renew_after > issued {
            prop_assert_eq!(evaluate(&licence, renew_after - 1, None), Status::Active);
        }
        // One second past expiry: grace, never still RenewalDue.
        let after_expiry = evaluate(&licence, expires_at + 1, None);
        prop_assert!(
            matches!(after_expiry, Status::Grace { .. }),
            "one past expiry: {after_expiry:?}"
        );
    }

    /// The grace window: within `expires_at + grace` still grace (inclusive end), one
    /// second past it restricted — for every grace the licence could carry.
    #[test]
    fn grace_window_ends_exactly(
        issued in 1_000_000_000i64..1_800_000_000,
        lifetime in 1i64..86_400 * 400,
        grace in 0u64..90 * 86_400,
    ) {
        let expires_at = issued.saturating_add(lifetime).min(2_240_000_000);
        let raw = RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: "0198c7a2-license-id".to_owned(),
            tenant: Some("acme".to_owned()),
            installation_id: None,
            edition: spot_license::model::Edition::new("business")
                .unwrap_or_else(|e| panic!("{e:?}")),
            ent: Entitlements::default(),
            kind: Kind::Subscription,
            issued_at: issued,
            not_before: issued,
            expires_at: Some(expires_at),
            renew_after: None,
            grace,
            serial: 1,
            key_id: "issuer-2026-q3".to_owned(),
        };
        let licence = Licence::new(raw.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        let grace_end = expires_at.saturating_add(i64::try_from(grace).unwrap_or(86_400 * 400));
        let at_end = evaluate(&licence, grace_end, None);
        let is_grace_with_end = matches!(at_end, Status::Grace { until } if until == grace_end);
        prop_assert!(is_grace_with_end, "at grace end: {at_end:?}");
        prop_assert_eq!(evaluate(&licence, grace_end + 1, None), Status::Restricted);
    }
}

/// The payload bytes a hostile file is made of: arbitrary, or a real licence's
/// canonical JSON with one byte flipped (exercises the parser's recovery paths, not
/// only its rejects-at-first-byte paths), or a truncation of one.
fn arbitrary_payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..512),
        (any::<usize>(), any::<u8>()).prop_map(|(index, byte)| {
            let mut bytes = subscription()
                .to_canonical_json()
                .unwrap_or_else(|e| panic!("{e:?}"))
                .into_bytes();
            let at = index % bytes.len();
            bytes[at] = byte;
            bytes
        }),
        (1usize..=8).prop_map(|cut| {
            let bytes = subscription()
                .to_canonical_json()
                .unwrap_or_else(|e| panic!("{e:?}"))
                .into_bytes();
            bytes[..bytes.len().saturating_sub(cut.min(bytes.len()))].to_vec()
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Tamper resistance as a property: no payload, signature or key id — well-formed,
    /// mutated or arbitrary — makes the envelope path or `verify` panic. It returns an
    /// error or a licence, always.
    #[test]
    fn verify_never_panics_on_arbitrary_bytes(
        payload in arbitrary_payload(),
        signature in proptest::collection::vec(any::<u8>(), 0..130),
        key_id in "(issuer|other|)\\w{0,20}",
    ) {
        let seed = {
            let mut seed = [7u8; 32];
            seed[0] = 1;
            seed
        };
        let verifying: VerifyingKey = SigningKey::from_bytes(&seed).verifying_key();
        let keys = TrustedKeys::new(vec![
            TrustedKey::from_bytes("issuer-2026-q3", &verifying.to_bytes(), 0, None)
                .unwrap_or_else(|e| panic!("{e:?}")),
        ]);
        // The envelope constructor itself must not panic on any of this either.
        if let Ok(signed) = SignedLicence::from_parts(payload, signature, key_id) {
            // Verification of a structurally plausible envelope: still no panic, and
            // the outcome is error-or-licence, never a third thing.
            let _outcome = verify(&signed, &keys);
        }
    }

    /// The same for `SignedLicence::from_text` over arbitrary text.
    #[test]
    fn from_text_never_panics_on_arbitrary_text(text in ".*") {
        let _ = SignedLicence::from_text(&text);
    }

    /// `Licence::from_json` never panics on arbitrary text; when it *does* produce a
    /// licence, a re-serialisation parses again (the model rules are stable).
    #[test]
    fn from_json_never_panics_on_arbitrary_text(text in ".*") {
        if let Ok(licence) = Licence::from_json(&text)
            && let Ok(canonical) = licence.to_canonical_json()
        {
            prop_assert!(Licence::from_json(&canonical).is_ok(), "re-parse broke");
        }
    }
}
