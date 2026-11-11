//! End-to-end licence flow tests (brief §7): the text form, every tamper path, key
//! expiry and rotation, the offline evaluation through the injected `Clock`, grace to
//! restricted, and the renewal flow with the clock guard.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself
//! built (cf. `spot-dag` tests); nothing here touches the system clock — the time
//! comes from `spot_core::ManualClock` through the `Clock` trait, exactly as the
//! product's callers are expected to drive it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use spot_core::ManualClock;
use spot_license::envelope::{LICENCE_TEXT_PREFIX, SignedLicence, TrustedKey, TrustedKeys, verify};
use spot_license::model::{
    Edition, Entitlements, Kind, Licence, Limit, PAYLOAD_VERSION, RawLicence,
};
use spot_license::renewal::apply_renewal;
use spot_license::state::{Status, evaluate};
use spot_license::{Action, ClockGuard, DEFAULT_CLOCK_TOLERANCE, DEFAULT_GRACE, Observation};

/// A throwaway signing key from a fixed seed; never a real issuer key.
fn signing_key(byte: u8) -> SigningKey {
    let mut seed = [byte; 32];
    seed[0] = 1;
    SigningKey::from_bytes(&seed)
}

/// A verified-trust key over a throwaway signing key.
fn trusted_key(
    key_id: &str,
    key: &SigningKey,
    valid_from: i64,
    valid_to: Option<i64>,
) -> TrustedKey {
    let public: VerifyingKey = key.verifying_key();
    TrustedKey::from_bytes(key_id, &public.to_bytes(), valid_from, valid_to)
        .unwrap_or_else(|error| panic!("{error:?}"))
}

/// A valid subscription licence for the tests of this module.
fn build_licence(key_id: &str, serial: u64) -> Licence {
    build_licence_issued(key_id, serial, 1_770_000_000)
}

/// A valid subscription licence with an explicit issue instant.
fn build_licence_issued(key_id: &str, serial: u64, issued_at: i64) -> Licence {
    Licence::new(RawLicence {
        v: PAYLOAD_VERSION,
        licence_id: "0198c7a2-license-id".to_owned(),
        tenant: Some("acme".to_owned()),
        installation_id: None,
        edition: Edition::new("business").unwrap_or_else(|e| panic!("{e:?}")),
        ent: Entitlements::from_lists(
            Some(Limit::UpTo(25)),
            vec!["bookkeeping".to_owned()],
            vec![],
        ),
        kind: Kind::Subscription,
        issued_at,
        not_before: issued_at,
        expires_at: Some(issued_at + 31 * 86_400),
        renew_after: Some(issued_at + 24 * 86_400),
        grace: DEFAULT_GRACE.as_secs(),
        serial,
        key_id: key_id.to_owned(),
    })
    .unwrap_or_else(|error| panic!("{error:?}"))
}

/// Signs `licence` with `key` and wraps it in an envelope.
fn envelope(licence: &Licence, key: &SigningKey) -> SignedLicence {
    let payload = licence
        .to_canonical_bytes()
        .unwrap_or_else(|e| panic!("{e:?}"));
    let signature = key.sign(&payload).to_bytes().to_vec();
    SignedLicence::from_parts(payload, signature, licence.issuer_key_id.clone())
        .unwrap_or_else(|error| panic!("{error:?}"))
}

/// The trust anchor for the single key `key` under `key_id`.
fn single_trust(key_id: &str, key: &SigningKey) -> TrustedKeys {
    TrustedKeys::new(vec![trusted_key(key_id, key, 0, None)])
}

#[test]
fn sign_verify_round_trip_through_the_text_form() {
    let key = signing_key(11);
    let key_id = "issuer-2026-q3";
    let licence = build_licence(key_id, 1);
    let signed = envelope(&licence, &key);

    // The text form travels as the file; the envelope comes back identical.
    let text = signed.to_text().unwrap_or_else(|e| panic!("{e:?}"));
    assert!(text.starts_with(LICENCE_TEXT_PREFIX));
    let travelled = SignedLicence::from_text(&text).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(travelled, signed);

    // Offline verification: no network, just the trusted list.
    let verified =
        verify(&travelled, &single_trust(key_id, &key)).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(verified, licence);
    // The round trip is exact down to the canonicity of the payload.
    let re_canonical = verified
        .to_canonical_json()
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(re_canonical.as_bytes(), signed.payload_bytes());
}

#[test]
fn every_payload_byte_flip_is_refused() {
    let key = signing_key(21);
    let key_id = "issuer-2026-q3";
    let licence = build_licence(key_id, 1);
    let signed = envelope(&licence, &key);
    let keys = single_trust(key_id, &key);
    let payload = signed.payload_bytes().to_vec();
    let signature = signed.signature_bytes().to_vec();

    for index in 0..payload.len() {
        let mut tampered = payload.clone();
        tampered[index] ^= 0b0000_0001;
        // A flipped payload byte either breaks the envelope's own parse (malformed
        // input) or survives parsing and must fail verification. Either way: no
        // licence.
        let outcome = SignedLicence::from_parts(tampered, signature.clone(), key_id.to_owned())
            .and_then(|tampered_signed| verify(&tampered_signed, &keys));
        assert!(outcome.is_err(), "payload byte {index} accepted");
    }
}

#[test]
fn every_signature_byte_flip_is_refused() {
    let key = signing_key(31);
    let key_id = "issuer-2026-q3";
    let licence = build_licence(key_id, 1);
    let signed = envelope(&licence, &key);
    let keys = single_trust(key_id, &key);
    let signature = signed.signature_bytes().to_vec();

    for index in 0..signature.len() {
        let mut tampered = signature.clone();
        tampered[index] ^= 0b1000_0000;
        let tampered_signed =
            SignedLicence::from_parts(signed.payload_bytes().to_vec(), tampered, key_id.to_owned())
                .unwrap_or_else(|e| panic!("signature byte {index}: {e:?}"));
        assert_eq!(
            verify(&tampered_signed, &keys).unwrap_err().code(),
            "licence.signature_invalid",
            "signature byte {index} accepted"
        );
    }
}

#[test]
fn tampered_key_id_is_refused() {
    let key = signing_key(41);
    let key_id = "issuer-2026-q3";
    let licence = build_licence(key_id, 1);
    let signed = envelope(&licence, &key);
    let keys = single_trust(key_id, &key);

    // The envelope naming another key than the payload (and the trust list) is refused
    // at once.
    let renamed = SignedLicence::from_parts(
        signed.payload_bytes().to_vec(),
        signed.signature_bytes().to_vec(),
        "other-key".to_owned(),
    );
    assert_eq!(renamed.unwrap_err().code(), "licence.key_mismatch");

    // A payload claiming another issuer key id does not verify against the trust list
    // that does not know it.
    let claim_other = build_licence("other-issuer-key", 1);
    let claim_other_signed = envelope(&claim_other, &key);
    let outcome = verify(&claim_other_signed, &keys);
    assert_eq!(
        outcome.unwrap_err().code(),
        "licence.key_unknown",
        "the trust list names the real key id, not the claimed one"
    );
    // For contrast: with the claimed key id actually in trust, verification succeeds.
    let honest = build_licence("other-issuer-key", 1);
    let honest_signed = envelope(&honest, &key);
    assert!(verify(&honest_signed, &single_trust("other-issuer-key", &key)).is_ok());
}

#[test]
fn non_canonical_payload_is_refused() {
    let key = signing_key(51);
    let key_id = "issuer-2026-q3";
    let licence = build_licence(key_id, 1);
    let canonical = licence
        .to_canonical_json()
        .unwrap_or_else(|e| panic!("{e:?}"));
    let keys = single_trust(key_id, &key);

    // A non-canonical mutation a hostile issuer could try. It still parses as the
    // same licence, so the *canonicity* check is what catches it; the signature over
    // the mutated bytes is genuine.
    let spaced = format!("{{ {}", &canonical[1..]);
    let payload = spaced.into_bytes();
    let signature = key.sign(&payload).to_bytes().to_vec();
    let signed = SignedLicence::from_parts(payload, signature, key_id.to_owned())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        verify(&signed, &keys).unwrap_err().code(),
        "licence.not_canonical",
        "a signed but non-canonical payload was accepted"
    );
}

#[test]
fn unknown_version_is_refused() {
    let key = signing_key(61);
    let key_id = "issuer-2026-q3";
    let licence = build_licence(key_id, 1);
    let canonical = licence
        .to_canonical_json()
        .unwrap_or_else(|e| panic!("{e:?}"));
    // "v":1 -> "v":2, same length so nothing else shifts.
    let bumped = canonical.replacen("\"v\":1", "\"v\":2", 1);
    assert_ne!(
        bumped, canonical,
        "the fixture must contain the version member"
    );
    let signature = key.sign(bumped.as_bytes()).to_bytes().to_vec();
    let signed = SignedLicence::from_parts(bumped.into_bytes(), signature, key_id.to_owned());
    // Either the payload itself is refused at parse, or verify refuses the version.
    match signed {
        Err(error) => assert_eq!(error.code(), "licence.json"),
        Ok(signed) => assert_eq!(
            verify(&signed, &single_trust(key_id, &key))
                .unwrap_err()
                .code(),
            "licence.unknown_version"
        ),
    }
}

#[test]
fn expired_key_is_refused() {
    let key = signing_key(71);
    let key_id = "issuer-2026-q3";
    // The key's trust ended before the licence was issued.
    let expired_list = TrustedKeys::new(vec![trusted_key(key_id, &key, 0, Some(1_760_000_000))]);
    let signed = envelope(&build_licence(key_id, 1), &key);
    assert_eq!(
        verify(&signed, &expired_list).unwrap_err().code(),
        "licence.key_expired"
    );

    // The same key with its window covering the issue instant verifies.
    let covering = TrustedKeys::new(vec![trusted_key(
        key_id,
        &key,
        1_760_000_000,
        Some(1_780_000_000),
    )]);
    assert!(verify(&signed, &covering).is_ok());

    // A key not yet trusted (window starts after the issue instant) is refused too.
    let future = TrustedKeys::new(vec![trusted_key(key_id, &key, 1_780_000_000, None)]);
    assert_eq!(
        verify(&signed, &future).unwrap_err().code(),
        "licence.key_expired"
    );
}

#[test]
fn key_rotation_moves_grants_between_windows() {
    // Rotation: key A issued the old grant, its window ended, key B took over with a
    // window that starts before B's licences are issued. Both keys are in the list —
    // an *old* licence still verifies while A's window covers its own issue instant,
    // and a new licence verifies under B.
    let old_key = signing_key(81);
    let new_key = signing_key(82);
    let old_id = "issuer-2026-q1";
    let new_id = "issuer-2026-q3";
    let both = TrustedKeys::new(vec![
        trusted_key(old_id, &old_key, 0, Some(1_770_500_000)),
        trusted_key(new_id, &new_key, 1_770_400_000, None),
    ]);

    let old_licence = build_licence_issued(old_id, 1, 1_770_000_000); // inside A's window
    let old_signed = envelope(&old_licence, &old_key);
    assert!(verify(&old_signed, &both).is_ok());

    // Key B's licences are issued inside B's window: minted at 1_770_450_000.
    let new_licence = build_licence_issued(new_id, 2, 1_770_450_000);
    let new_signed = envelope(&new_licence, &new_key);
    assert!(
        verify(&new_signed, &both).is_ok(),
        "the new key's licence verifies under B"
    );

    // A new licence minted with the *old* key inside the old window is still fine…
    let also_old = build_licence_issued(old_id, 3, 1_770_450_000);
    assert!(verify(&envelope(&also_old, &old_key), &both).is_ok());

    // …but dropping the old key from the list revokes it.
    let only_new = TrustedKeys::new(vec![trusted_key(new_id, &new_key, 0, None)]);
    assert_eq!(
        verify(&old_signed, &only_new).unwrap_err().code(),
        "licence.key_unknown"
    );
    // And the new licence survives.
    assert!(verify(&new_signed, &only_new).is_ok());
}

/// An `OffsetDateTime` at the UTC epoch second, for the manual clock's starts.
fn offset_at(unix_second: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(unix_second).unwrap_or_else(|e| panic!("{e:?}"))
}

#[test]
fn grace_then_restricted_offline_through_the_clock() {
    // The WP acceptance story: a desktop install renews offline inside the grace
    // period. The clock is the injected `spot_core::ManualClock`; no system time is
    // read anywhere in the crate.
    use spot_core::Clock as _;

    let key = signing_key(91);
    let key_id = "issuer-2026-q3";
    let keys = single_trust(key_id, &key);
    let licence = build_licence(key_id, 1);
    let verified = verify(&envelope(&licence, &key), &keys).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        spot_core::Clock::now_utc(&ManualClock::new(offset_at(1_770_000_000))).unix_timestamp(),
        1_770_000_000
    );

    let expires_at = verified.expires_at.unwrap_or(0);
    let grace_seconds: i64 = i64::try_from(verified.grace.as_secs()).unwrap_or(i64::MAX);

    let clock = ManualClock::new(offset_at(1_770_000_000));
    let now = || clock.now_utc().unix_timestamp();
    let advance = |seconds: u64| clock.advance(std::time::Duration::from_secs(seconds));

    // Active well before anything.
    assert_eq!(evaluate(&verified, now(), None), Status::Active);
    // Into grace: one second past expiry.
    advance(u64::try_from(expires_at - 1_770_000_000 + 1).unwrap_or(u64::MAX));
    assert_eq!(
        evaluate(&verified, now(), None),
        Status::Grace {
            until: expires_at + grace_seconds
        }
    );
    // Full function in grace: renew works, documents work.
    assert!(
        Status::Grace { until: 0 }
            .allows(Action::CreateDocument)
            .is_allow()
    );
    assert!(
        Status::Grace { until: 0 }
            .allows(Action::InstallApp)
            .is_allow()
    );

    // Past the grace, restricted: read-only.
    advance(verified.grace.as_secs() + 1);
    assert_eq!(evaluate(&verified, now(), None), Status::Restricted);
    assert!(
        !evaluate(&verified, now(), None)
            .allows(Action::CreateDocument)
            .is_allow()
    );
    assert!(
        evaluate(&verified, now(), None)
            .allows(Action::ExportData)
            .is_allow()
    );
}

#[test]
fn renewal_flow_replaces_a_stale_grace_licence() {
    // The licence expired into grace; the control plane's renewal (signed with a
    // newer serial) lands, is verified offline, and is applied — after which the
    // install is Active again and the clock guard is re-anchored.
    let key = signing_key(101);
    let key_id = "issuer-2026-q3";
    // Serial 1 issued later than the plain fixture: expired at 1_772_678_400 - the
    // numbers line up with the grace arithmetic assertions below.
    let serial_1 = build_licence_issued(key_id, 1, 1_770_000_000);
    let mut raw_renewal = serde_json::from_str::<RawLicence>(
        &serial_1
            .to_canonical_json()
            .unwrap_or_else(|e| panic!("{e:?}")),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    raw_renewal.serial = 2;
    raw_renewal.expires_at = Some(1_775_400_000); // another month
    raw_renewal.not_before = 1_770_000_000; // already valid; the renewal applies now
    raw_renewal.renew_after = None;
    let serial_2 = Licence::new(raw_renewal).unwrap_or_else(|e| panic!("{e:?}"));

    let trusted = TrustedKeys::new(vec![trusted_key(key_id, &key, 0, None)]);
    let incoming = verify(&envelope(&serial_2, &key), &trusted).unwrap_or_else(|e| panic!("{e:?}"));

    // Evaluation in grace for serial 1… (expires 1_772_678_400; default grace
    // 604_800 s puts the grace end at 1_773_283_200.)
    let grace_now = 1_772_700_000; // inside the grace of the first expiry
    assert_eq!(
        evaluate(&serial_1, grace_now, None),
        Status::Grace {
            until: 1_772_678_400 + 604_800
        }
    );

    // …then the renewal applies and evaluation is plain again.
    let applied = apply_renewal(&serial_1, &incoming).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(applied.serial, 2);
    assert_eq!(evaluate(&applied, grace_now, None), Status::Active);

    // A replay of the *old* licence after the renewal is a replay: stale.
    let replayed = apply_renewal(&applied, &serial_1);
    assert_eq!(replayed.unwrap_err().code(), "licence.stale");

    // And the guard: a suspect clock during the lapse is cleared by the renewal.
    let mut guard = ClockGuard::new(
        Some(Observation {
            high_water_mark: 1_772_700_000,
            monotonic_seconds: 5,
        }),
        DEFAULT_CLOCK_TOLERANCE,
    );
    let suspect = guard.evaluate(&serial_1, 1_772_700_000 - 30 * 86_400, 6, None);
    assert!(matches!(suspect.status, Status::ClockSuspect { .. }));
    let _anchored = guard.reanchor(1_772_700_000, 7);
    let after = guard.evaluate(&applied, 1_772_700_000, 7, None);
    assert_eq!(after.status, Status::Active);
    assert!(!matches!(after.status, Status::ClockSuspect { .. }));
}

#[test]
fn trial_lifecycle_offline_through_the_clock() {
    let key = signing_key(111);
    let key_id = "issuer-2026-q3";
    // A trial directly: no renew_after, no grace, an install as the holder.
    let trial = Licence::new(RawLicence {
        v: PAYLOAD_VERSION,
        licence_id: "trial-0001".to_owned(),
        tenant: None,
        installation_id: Some("install-7f3a".to_owned()),
        edition: Edition::new("starter").unwrap_or_else(|e| panic!("{e:?}")),
        ent: Entitlements::none(),
        kind: Kind::Trial,
        issued_at: 1_770_000_000,
        not_before: 1_770_000_000,
        expires_at: Some(1_771_209_600), // 14 days
        renew_after: None,
        grace: 0,
        serial: 1,
        key_id: key_id.to_owned(),
    })
    .unwrap_or_else(|e| panic!("{e:?}"));
    let keys = single_trust(key_id, &key);
    let verified = verify(&envelope(&trial, &key), &keys).unwrap_or_else(|e| panic!("{e:?}"));

    assert_eq!(evaluate(&verified, 1_771_209_600, None), Status::Active);
    assert_eq!(
        evaluate(&verified, 1_771_209_600 + 1, None),
        Status::Expired
    );
    // No grace for trials, not even a second of it.
    assert_eq!(
        evaluate(&verified, 1_771_209_600 + 7 * 86_400, None),
        Status::Expired
    );
}
