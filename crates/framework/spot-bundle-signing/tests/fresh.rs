//! The fresh revocation list (domain V2): issue, wire codec, verification, freshness, replay,
//! tampering, the independent rebuild from the contract text, and parser robustness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_signing::canonical::b64;
use spot_bundle_signing::fresh::{
    MAX_GENERATION, MAX_LIST_LIFETIME_SECONDS, format_rfc3339, parse_rfc3339,
};
use spot_bundle_signing::*;

/// 2026-10-31T00:00:00Z.
const ISSUED: i64 = 1_793_404_800;
/// Six hours later: 2026-10-31T06:00:00Z.
const NEXT: i64 = ISSUED + 6 * 3600;
const NOW: i64 = ISSUED + 600;

fn digest_a() -> String {
    "ab".repeat(32)
}

fn digest_b() -> String {
    "01".repeat(32)
}

/// A body with every list populated, deliberately out of order.
fn body() -> FreshRevocationBody {
    FreshRevocationBody {
        generation: 7,
        issued_at: ISSUED,
        next_update: NEXT,
        issued_by: root().public().key_id(),
        keys: vec![
            RevokedKey {
                key_id: other_key().public().key_id(),
                effective_from: ISSUED - 3600,
                compromised: true,
                reason: "leaked \"key\" \u{e9}".to_owned(),
            },
            RevokedKey {
                key_id: publisher().public().key_id(),
                effective_from: 0,
                compromised: false,
                reason: "rotated".to_owned(),
            },
        ],
        publishers: vec![
            RevokedPublisher {
                publisher_id: "zeta-2".to_owned(),
                effective_from: ISSUED - 86_400,
                reason: "abuse".to_owned(),
            },
            RevokedPublisher {
                publisher_id: "acme".to_owned(),
                effective_from: 951_782_400,
                reason: "takedown".to_owned(),
            },
        ],
        bundles: vec![
            RevokedBundle {
                digest_hex: digest_a(),
                reason: "malware".to_owned(),
            },
            RevokedBundle {
                digest_hex: digest_b(),
                reason: "legal".to_owned(),
            },
        ],
    }
}

fn issued() -> FreshRevocationList {
    issue_fresh_revocation_list(body(), &root()).unwrap()
}

fn anchor_of(key: &KeyPair) -> TrustAnchor {
    TrustAnchor::new(vec![TrustedRoot::new(key.public(), 0, None)])
}

fn verify_at(list: &FreshRevocationList, now: i64, held: Option<u64>) -> Result<(), SigningError> {
    verify_fresh_revocation_list(list, &anchor_of(&root()), now, held)
}

// --- the independent rebuild, from the contract text only ---------------------------------------

fn js(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

/// The signed bytes as `contracts/store-api` `revocations_get` describes them: the ASCII domain
/// `SPOT-REVOCATION-LIST-V2\n`, then compact UTF-8 JSON of an object with `bundles`,
/// `generation`, `issued_at`, `issued_by`, `keys`, `next_update`, `publishers` (keys of every
/// object in lexicographic order), integer UTC seconds for times, entries in the order given.
/// Built as plain strings from the wire JSON, using nothing of the crate.
fn independent_rebuild(wire: &str) -> Vec<u8> {
    let v: serde_json::Value = serde_json::from_str(wire).unwrap();
    let secs = |t: &serde_json::Value| -> i64 {
        let t = t.as_str().unwrap();
        // YYYY-MM-DDTHH:MM:SSZ, converted with a second, different algorithm: count days by
        // walking the years and months.
        let n = |a: usize, b: usize| t[a..b].parse::<i64>().unwrap();
        let (y, mo, d) = (n(0, 4), n(5, 7), n(8, 10));
        let mut days = 0i64;
        for year in 1970..y {
            days += if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                366
            } else {
                365
            };
        }
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let months = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        days += months
            .iter()
            .take(usize::try_from(mo - 1).unwrap())
            .sum::<i64>();
        days += d - 1;
        days * 86_400 + n(11, 13) * 3600 + n(14, 16) * 60 + n(17, 19)
    };
    let list = |name: &str, f: &dyn Fn(&serde_json::Value) -> String| -> String {
        let items: Vec<String> = v[name].as_array().unwrap().iter().map(f).collect();
        format!("[{}]", items.join(","))
    };
    let keys = list("keys", &|k| {
        format!(
            "{{\"compromised\":{},\"effective_from\":{},\"key_id\":{},\"reason\":{}}}",
            k["compromised"].as_bool().unwrap(),
            secs(&k["effective_from"]),
            js(k["key_id"].as_str().unwrap()),
            js(k["reason"].as_str().unwrap())
        )
    });
    let publishers = list("publishers", &|p| {
        format!(
            "{{\"effective_from\":{},\"publisher_id\":{},\"reason\":{}}}",
            secs(&p["effective_from"]),
            js(p["publisher_id"].as_str().unwrap()),
            js(p["reason"].as_str().unwrap())
        )
    });
    let bundles = list("bundles", &|b| {
        format!(
            "{{\"digest\":{},\"reason\":{}}}",
            js(b["digest"].as_str().unwrap()),
            js(b["reason"].as_str().unwrap())
        )
    });
    let text = format!(
        "SPOT-REVOCATION-LIST-V2\n{{\"bundles\":{bundles},\"generation\":{},\"issued_at\":{},\"issued_by\":{},\"keys\":{keys},\"next_update\":{},\"publishers\":{publishers}}}",
        v["generation"].as_u64().unwrap(),
        secs(&v["issued_at"]),
        js(v["issued_by"].as_str().unwrap()),
        secs(&v["next_update"]),
    );
    text.into_bytes()
}

#[test]
fn the_independent_rebuild_equals_the_crates_bytes_and_the_signature_verifies_over_it() {
    let list = issued();
    let wire = list.to_wire_json().unwrap();
    let rebuilt = independent_rebuild(&wire);
    assert_eq!(rebuilt, list.body.signed_bytes());
    // The signature verifies over the independently built bytes.
    root().public().verify(&rebuilt, list.signature()).unwrap();
}

// --- golden vector ------------------------------------------------------------------------------

#[test]
fn the_v2_bytes_and_signature_are_pinned() {
    let list = issued();
    assert_eq!(
        String::from_utf8(list.body.signed_bytes()).unwrap(),
        GOLDEN_BYTES
    );
    assert_eq!(b64(list.signature()), GOLDEN_SIGNATURE);
    assert_eq!(list.to_wire_json().unwrap(), GOLDEN_WIRE);
}

const GOLDEN_BYTES: &str = "SPOT-REVOCATION-LIST-V2\n{\"bundles\":[{\"digest\":\"0101010101010101010101010101010101010101010101010101010101010101\",\"reason\":\"legal\"},{\"digest\":\"abababababababababababababababababababababababababababababababab\",\"reason\":\"malware\"}],\"generation\":7,\"issued_at\":1793404800,\"issued_by\":\"34750f98bd59fcfc946da45aaabe933b\",\"keys\":[{\"compromised\":false,\"effective_from\":0,\"key_id\":\"6a3803d5f059902a1c6dafbc9ba47292\",\"reason\":\"rotated\"},{\"compromised\":true,\"effective_from\":1793401200,\"key_id\":\"b62e867fa2f33afe62d5d6b1642e1621\",\"reason\":\"leaked \\\"key\\\" é\"}],\"next_update\":1793426400,\"publishers\":[{\"effective_from\":951782400,\"publisher_id\":\"acme\",\"reason\":\"takedown\"},{\"effective_from\":1793318400,\"publisher_id\":\"zeta-2\",\"reason\":\"abuse\"}]}";
const GOLDEN_SIGNATURE: &str =
    "5naGBPBRz9g3k2Y0nkauRq8TJKjMAHp+RcNjSekaE8v3UaxLohh2rc60f/6ZROWpap1BHFc0s+UeYp9xjIqTCw==";
const GOLDEN_WIRE: &str = "{\"generation\":7,\"issued_at\":\"2026-10-31T00:00:00Z\",\"next_update\":\"2026-10-31T06:00:00Z\",\"issued_by\":\"34750f98bd59fcfc946da45aaabe933b\",\"signature\":\"5naGBPBRz9g3k2Y0nkauRq8TJKjMAHp+RcNjSekaE8v3UaxLohh2rc60f/6ZROWpap1BHFc0s+UeYp9xjIqTCw==\",\"keys\":[{\"key_id\":\"6a3803d5f059902a1c6dafbc9ba47292\",\"effective_from\":\"1970-01-01T00:00:00Z\",\"compromised\":false,\"reason\":\"rotated\"},{\"key_id\":\"b62e867fa2f33afe62d5d6b1642e1621\",\"effective_from\":\"2026-10-30T23:00:00Z\",\"compromised\":true,\"reason\":\"leaked \\\"key\\\" é\"}],\"publishers\":[{\"publisher_id\":\"acme\",\"effective_from\":\"2000-02-29T00:00:00Z\",\"reason\":\"takedown\"},{\"publisher_id\":\"zeta-2\",\"effective_from\":\"2026-10-30T00:00:00Z\",\"reason\":\"abuse\"}],\"bundles\":[{\"digest\":\"0101010101010101010101010101010101010101010101010101010101010101\",\"reason\":\"legal\"},{\"digest\":\"abababababababababababababababababababababababababababababababab\",\"reason\":\"malware\"}]}";

// --- issue, wire, verify ------------------------------------------------------------------------

#[test]
fn issue_sorts_entries_and_the_round_trip_verifies() {
    let list = issued();
    let b = &list.body;
    assert!(b.keys.windows(2).all(|w| w[0].key_id < w[1].key_id));
    assert!(
        b.publishers
            .windows(2)
            .all(|w| w[0].publisher_id < w[1].publisher_id)
    );
    assert!(
        b.bundles
            .windows(2)
            .all(|w| w[0].digest_hex < w[1].digest_hex)
    );
    verify_at(&list, NOW, None).unwrap();
    let wire = list.to_wire_json().unwrap();
    let parsed = FreshRevocationList::from_wire_json(wire.as_bytes()).unwrap();
    assert_eq!(parsed, list);
    verify_at(&parsed, NOW, None).unwrap();
    // Equal content signs the same bytes whatever order the caller gave.
    let mut reversed = body();
    reversed.keys.reverse();
    reversed.bundles.reverse();
    assert_eq!(
        issue_fresh_revocation_list(reversed, &root()).unwrap(),
        list
    );
}

#[test]
fn the_wire_form_is_the_contracts_json() {
    let wire = issued().to_wire_json().unwrap();
    let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
    let object = v.as_object().unwrap();
    let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "bundles",
            "generation",
            "issued_at",
            "issued_by",
            "keys",
            "next_update",
            "publishers",
            "signature"
        ]
    );
    assert_eq!(v["issued_at"], "2026-10-31T00:00:00Z");
    assert_eq!(v["next_update"], "2026-10-31T06:00:00Z");
    assert!(v["generation"].is_u64());
    assert!(v["bundles"][0].get("digest").is_some());
    assert!(v["bundles"][0].get("digest_hex").is_none());
    assert!(
        !wire.contains(
            "
"
        ) && !wire.contains("\": ")
    );
}

#[test]
fn the_v1_signature_is_not_a_v2_signature() {
    // The same fields signed under V1 (no next_update, other domain) never verify as V2.
    let v1 = issue_revocation_list(
        RevocationBody {
            generation: 7,
            issued_at: ISSUED,
            issued_by: root().public().key_id(),
            keys: vec![],
            publishers: vec![],
            bundles: vec![],
        },
        &root(),
    );
    let mut b = body();
    b.keys.clear();
    b.publishers.clear();
    b.bundles.clear();
    let forged = FreshRevocationList::from_parts(b, *v1.signature());
    assert_eq!(
        verify_at(&forged, NOW, None),
        Err(SigningError::RevocationSignatureInvalid)
    );
}

#[test]
fn tampering_with_any_field_fails_verification() {
    let list = issued();
    let mut cases: Vec<(&str, FreshRevocationBody)> = Vec::new();
    let mut add = |name, f: &dyn Fn(&mut FreshRevocationBody)| {
        let mut b = list.body.clone();
        f(&mut b);
        cases.push((name, b));
    };
    add("generation", &|b| b.generation += 1);
    add("issued_at", &|b| b.issued_at += 1);
    add("next_update", &|b| b.next_update += 1);
    add("next_update extended", &|b| b.next_update += 3600);
    add("key id", &|b| b.keys[0].key_id = root().public().key_id());
    add("key effective_from", &|b| b.keys[0].effective_from += 1);
    add("key compromised", &|b| {
        b.keys[0].compromised = !b.keys[0].compromised
    });
    add("key reason", &|b| b.keys[0].reason.push('x'));
    add("publisher id", &|b| {
        b.publishers[0].publisher_id = "evil".to_owned()
    });
    add("publisher effective_from", &|b| {
        b.publishers[0].effective_from += 1
    });
    add("publisher reason", &|b| b.publishers[0].reason.push('x'));
    add("bundle digest", &|b| {
        b.bundles[0].digest_hex = "cd".repeat(32)
    });
    add("bundle reason", &|b| b.bundles[0].reason.push('x'));
    add("key removed", &|b| {
        b.keys.pop();
    });
    add("publisher removed", &|b| {
        b.publishers.pop();
    });
    add("bundle removed", &|b| {
        b.bundles.pop();
    });
    add("bundle added", &|b| {
        b.bundles.push(RevokedBundle {
            digest_hex: "ee".repeat(32),
            reason: "x".to_owned(),
        });
    });
    add("order swapped", &|b| b.keys.swap(0, 1));
    for (name, b) in cases {
        let tampered = FreshRevocationList::from_parts(b, *list.signature());
        assert_eq!(
            verify_at(&tampered, NOW, None),
            Err(SigningError::RevocationSignatureInvalid),
            "{name}"
        );
    }
    // The issuer field: a different key id is not in the anchor.
    let mut b = list.body.clone();
    b.issued_by = other_key().public().key_id();
    assert_eq!(
        verify_at(
            &FreshRevocationList::from_parts(b, *list.signature()),
            NOW,
            None
        ),
        Err(SigningError::UnknownRoot)
    );
    // A flipped signature bit.
    let mut sig = *list.signature();
    sig[10] ^= 1;
    assert_eq!(
        verify_at(
            &FreshRevocationList::from_parts(list.body.clone(), sig),
            NOW,
            None
        ),
        Err(SigningError::RevocationSignatureInvalid)
    );
}

#[test]
fn a_replayed_lower_generation_is_refused_and_an_equal_one_is_accepted() {
    let list = issued();
    assert_eq!(
        verify_at(&list, NOW, Some(8)),
        Err(SigningError::StaleRevocationList)
    );
    verify_at(&list, NOW, Some(7)).unwrap();
    verify_at(&list, NOW, Some(6)).unwrap();
    verify_at(&list, NOW, Some(0)).unwrap();
    verify_at(&list, NOW, None).unwrap();
}

#[test]
fn a_stale_list_is_refused_and_is_stale_says_so() {
    let list = issued();
    assert!(!list.is_stale(NEXT - 1));
    assert!(list.is_stale(NEXT));
    assert!(list.is_stale(NEXT + 1));
    verify_at(&list, NEXT - 1, None).unwrap();
    assert_eq!(
        verify_at(&list, NEXT, None),
        Err(SigningError::RevocationListExpired)
    );
    assert_eq!(
        verify_at(&list, NEXT + 86_400, None),
        Err(SigningError::RevocationListExpired)
    );
}

#[test]
fn an_unknown_or_out_of_window_issuer_is_refused() {
    let list = issued();
    assert_eq!(
        verify_fresh_revocation_list(&list, &anchor_of(&other_key()), NOW, None),
        Err(SigningError::UnknownRoot)
    );
    assert_eq!(
        verify_fresh_revocation_list(&list, &TrustAnchor::default(), NOW, None),
        Err(SigningError::UnknownRoot)
    );
    let closed = TrustAnchor::new(vec![TrustedRoot::new(root().public(), 0, Some(NOW))]);
    assert_eq!(
        verify_fresh_revocation_list(&list, &closed, NOW, None),
        Err(SigningError::RootOutsideWindow)
    );
    let not_yet = TrustAnchor::new(vec![TrustedRoot::new(root().public(), NOW + 1, None)]);
    assert_eq!(
        verify_fresh_revocation_list(&list, &not_yet, NOW, None),
        Err(SigningError::RootOutsideWindow)
    );
}

#[test]
fn issue_refuses_a_bad_lifetime_a_wrong_signer_and_a_bad_body() {
    let mut b = body();
    b.next_update = b.issued_at;
    assert_eq!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::RevocationLifetimeInvalid)
    );
    let mut b = body();
    b.next_update = b.issued_at - 1;
    assert_eq!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::RevocationLifetimeInvalid)
    );
    let mut b = body();
    b.next_update = b.issued_at + MAX_LIST_LIFETIME_SECONDS;
    issue_fresh_revocation_list(b, &root()).unwrap();
    let mut b = body();
    b.next_update = b.issued_at + MAX_LIST_LIFETIME_SECONDS + 1;
    assert_eq!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::RevocationLifetimeInvalid)
    );
    assert_eq!(
        issue_fresh_revocation_list(body(), &other_key()),
        Err(SigningError::KeyMismatch)
    );
    let mut b = body();
    b.generation = MAX_GENERATION + 1;
    assert!(matches!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::DocumentMalformed { .. })
    ));
    let mut b = body();
    b.generation = MAX_GENERATION;
    issue_fresh_revocation_list(b, &root()).unwrap();
    let mut b = body();
    b.bundles[0].digest_hex = "ABC".to_owned();
    assert!(matches!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::DocumentMalformed { .. })
    ));
    let mut b = body();
    b.publishers[0].publisher_id = "a b".to_owned();
    assert!(matches!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::DocumentMalformed { .. })
    ));
    let mut b = body();
    b.keys[0].reason = "x".repeat(2001);
    assert!(matches!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::DocumentMalformed { .. })
    ));
    let mut b = body();
    b.keys[0].effective_from = 253_402_300_800;
    assert!(matches!(
        issue_fresh_revocation_list(b, &root()),
        Err(SigningError::DocumentMalformed { .. })
    ));
}

#[test]
fn verify_refuses_a_correctly_signed_list_longer_than_seven_days() {
    // A signer that signs whatever it is given: the verifier must still refuse.
    let mut b = body();
    b.next_update = b.issued_at + MAX_LIST_LIFETIME_SECONDS + 1;
    let signature = RevocationSigner::sign(&root(), &b.signed_bytes());
    let list = FreshRevocationList::from_parts(b, signature);
    assert_eq!(
        verify_at(&list, NOW, None),
        Err(SigningError::RevocationLifetimeInvalid)
    );
    // And a list whose next_update is not after issued_at.
    let mut b = body();
    b.next_update = b.issued_at;
    let signature = RevocationSigner::sign(&root(), &b.signed_bytes());
    assert_eq!(
        verify_at(&FreshRevocationList::from_parts(b, signature), NOW, None),
        Err(SigningError::RevocationLifetimeInvalid)
    );
}

#[test]
fn the_signer_port_is_the_key_pair_and_a_custom_signer_works() {
    struct Wrapper(KeyPair);
    impl RevocationSigner for Wrapper {
        fn key_id(&self) -> &KeyId {
            RevocationSigner::key_id(&self.0)
        }
        fn sign(&self, message: &[u8]) -> [u8; 64] {
            RevocationSigner::sign(&self.0, message)
        }
    }
    let by_port = issue_fresh_revocation_list(body(), &Wrapper(root())).unwrap();
    assert_eq!(by_port, issued());
    assert_eq!(RevocationSigner::key_id(&root()), &root().public().key_id());
    // The key pair never prints its secret.
    let printed = format!("{:?}", root());
    assert!(printed.contains("<redacted>"));
}

// --- the time codec -----------------------------------------------------------------------------

#[test]
fn rfc3339_known_instants_round_trip() {
    let known: [(i64, &str); 10] = [
        (0, "1970-01-01T00:00:00Z"),
        (-1, "1969-12-31T23:59:59Z"),
        (951_782_400, "2000-02-29T00:00:00Z"),
        (1_793_404_800, "2026-10-31T00:00:00Z"),
        (4_294_967_295, "2106-02-07T06:28:15Z"),
        (4_294_967_296, "2106-02-07T06:28:16Z"),
        (-62_167_219_200, "0000-01-01T00:00:00Z"),
        (253_402_300_799, "9999-12-31T23:59:59Z"),
        (951_868_800, "2000-03-01T00:00:00Z"),
        (86_399, "1970-01-01T23:59:59Z"),
    ];
    for (seconds, text) in known {
        assert_eq!(format_rfc3339(seconds).unwrap(), text);
        assert_eq!(parse_rfc3339(text).unwrap(), seconds);
    }
    assert!(format_rfc3339(253_402_300_800).is_err());
    assert!(format_rfc3339(-62_167_219_201).is_err());
}

#[test]
fn rfc3339_round_trips_across_the_whole_range() {
    let mut s = -62_167_219_200i64;
    while s <= 253_402_300_799 {
        let text = format_rfc3339(s).unwrap();
        assert_eq!(parse_rfc3339(&text).unwrap(), s, "{text}");
        s += 86_400 * 37 + 13;
    }
    // Every second around the leap days and the year boundaries.
    for center in [951_782_400i64, 4_107_456_000, 0, 4_294_967_296, 946_684_800] {
        for s in center - 90_000..center + 90_000 {
            assert_eq!(parse_rfc3339(&format_rfc3339(s).unwrap()).unwrap(), s);
        }
    }
}

#[test]
fn rfc3339_leap_years_follow_the_gregorian_rule() {
    for ok in ["2000-02-29", "2024-02-29", "2400-02-29", "1972-02-29"] {
        parse_rfc3339(&format!("{ok}T00:00:00Z")).unwrap();
    }
    for bad in [
        "1900-02-29",
        "2100-02-29",
        "2023-02-29",
        "2026-02-30",
        "2026-04-31",
        "2026-13-01",
        "2026-00-10",
        "2026-01-00",
        "2026-01-32",
    ] {
        assert!(parse_rfc3339(&format!("{bad}T00:00:00Z")).is_err(), "{bad}");
    }
}

#[test]
fn rfc3339_refuses_every_other_form() {
    for bad in [
        "",
        "2026-10-31T00:00:00",
        "2026-10-31T00:00:00z",
        "2026-10-31t00:00:00Z",
        "2026-10-31 00:00:00Z",
        "2026-10-31T00:00:00+00:00",
        "2026-10-31T00:00:00-00:00",
        "2026-10-31T00:00:00.5Z",
        "2026-10-31T00:00:00.000Z",
        "2026-10-31T00:00:60Z",
        "2026-10-31T00:60:00Z",
        "2026-10-31T24:00:00Z",
        "10000-01-01T00:00:00Z",
        "+2026-10-31T00:00:00Z",
        "2026-10-31T00:00:00ZZ",
        " 2026-10-31T00:00:00Z",
        "2026-10-31T00:00:0\u{0663}Z",
        "2026/10/31T00:00:00Z",
        "20261031T000000Z",
        "-001-01-01T00:00:00Z",
    ] {
        assert!(parse_rfc3339(bad).is_err(), "{bad:?}");
    }
}

// --- the strict parser --------------------------------------------------------------------------

fn wire_value() -> serde_json::Value {
    serde_json::from_str(&issued().to_wire_json().unwrap()).unwrap()
}

fn parse(v: &serde_json::Value) -> Result<FreshRevocationList, SigningError> {
    FreshRevocationList::from_wire_json(v.to_string().as_bytes())
}

fn parse_text(text: &str) -> Result<FreshRevocationList, SigningError> {
    FreshRevocationList::from_wire_json(text.as_bytes())
}

#[test]
fn the_parser_refuses_unknown_missing_and_mistyped_fields() {
    let mut v = wire_value();
    v["extra"] = 1.into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["keys"][0]["extra"] = 1.into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["bundles"][0]["digest_hex"] = "x".into();
    assert!(parse(&v).is_err());
    for field in [
        "generation",
        "issued_at",
        "next_update",
        "issued_by",
        "signature",
        "keys",
        "publishers",
        "bundles",
    ] {
        let mut v = wire_value();
        v.as_object_mut().unwrap().remove(field);
        assert!(parse(&v).is_err(), "missing {field}");
    }
    let mut v = wire_value();
    v["generation"] = "7".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["keys"][0]["compromised"] = "yes".into();
    assert!(parse(&v).is_err());
}

#[test]
fn the_parser_refuses_duplicate_keys() {
    let text = issued().to_wire_json().unwrap();
    let dup_top = text.replacen('{', "{\"generation\":9,", 1);
    assert!(parse_text(&dup_top).is_err());
    let dup_nested = text.replacen("\"reason\"", "\"reason\":\"a\",\"reason\"", 1);
    assert!(parse_text(&dup_nested).is_err());
}

#[test]
fn the_parser_refuses_bad_times_numbers_and_trailing_text() {
    let mut v = wire_value();
    v["issued_at"] = "2026-10-31T00:00:00+00:00".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["next_update"] = "2026-10-31T06:00:00.5Z".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["keys"][0]["effective_from"] = "2026-02-30T00:00:00Z".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["publishers"][0]["effective_from"] = 951_782_400.into();
    assert!(parse(&v).is_err());
    // generation: a float, a negative, 2^53 + 1, and 2^53 exactly.
    for bad in [
        "7.0",
        "-1",
        "9007199254740993",
        "1e3",
        "1E400",
        "00007",
        "+7",
    ] {
        let text = issued().to_wire_json().unwrap().replacen(
            "\"generation\":7",
            &format!("\"generation\":{bad}"),
            1,
        );
        assert!(parse_text(&text).is_err(), "{bad}");
    }
    let text = issued().to_wire_json().unwrap().replacen(
        "\"generation\":7",
        "\"generation\":9007199254740992",
        1,
    );
    assert_eq!(parse_text(&text).unwrap().body.generation, MAX_GENERATION);
    let text = issued().to_wire_json().unwrap();
    assert!(parse_text(&format!("{text} x")).is_err());
    assert!(parse_text(&format!("{text}{text}")).is_err());
    assert!(parse_text(&format!("\u{feff}{text}")).is_err());
    assert!(parse_text("").is_err());
    assert!(parse_text("null").is_err());
    assert!(parse_text("[]").is_err());
}

#[test]
fn the_parser_refuses_a_bad_signature_and_bad_ids() {
    let mut v = wire_value();
    v["signature"] = "!!!".into();
    assert_eq!(parse(&v), Err(SigningError::SignatureMalformed));
    let mut v = wire_value();
    v["signature"] = b64(&[0u8; 63]).into();
    assert_eq!(parse(&v), Err(SigningError::SignatureMalformed));
    let mut v = wire_value();
    v["issued_by"] = "ABC".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["keys"][0]["key_id"] = "short".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["publishers"][0]["publisher_id"] = "not ok!".into();
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["bundles"][0]["digest"] = "AB".repeat(32).into();
    assert!(parse(&v).is_err());
    // A wire list whose lifetime is over 7 days is refused at parse.
    let mut v = wire_value();
    v["next_update"] = "2026-11-08T00:00:00Z".into();
    assert_eq!(parse(&v), Err(SigningError::RevocationLifetimeInvalid));
}

#[test]
fn the_signed_bytes_come_from_the_parsed_value_not_the_received_text() {
    // Same value, different text (whitespace, key order): still verifies.
    let v = wire_value();
    let pretty = serde_json::to_string_pretty(&v).unwrap();
    let parsed = parse_text(&pretty).unwrap();
    verify_at(&parsed, NOW, None).unwrap();
    assert_eq!(parsed, issued());
    // Changing next_update in the text changes the rebuilt bytes: the signature fails.
    let mut v = wire_value();
    v["next_update"] = "2026-11-07T00:00:00Z".into();
    let parsed = parse(&v).unwrap();
    assert_eq!(
        verify_at(&parsed, NOW, None),
        Err(SigningError::RevocationSignatureInvalid)
    );
}

#[test]
fn the_parser_survives_hostile_input() {
    let good = issued().to_wire_json().unwrap();
    // Huge numbers.
    for n in ["9".repeat(5000), format!("1e{}", "9".repeat(500))] {
        assert!(
            parse_text(&good.replacen("\"generation\":7", &format!("\"generation\":{n}"), 1))
                .is_err()
        );
    }
    // Deep nesting.
    let deep = format!("{}{}", "[".repeat(200_000), "]".repeat(200_000));
    assert!(parse_text(&deep).is_err());
    let deep_field = good.replacen(
        "\"keys\":[",
        &format!("\"keys\":[{}", "[".repeat(100_000)),
        1,
    );
    assert!(parse_text(&deep_field).is_err());
    let deep_obj = format!("{}1{}", "{\"a\":".repeat(50_000), "}".repeat(50_000));
    assert!(parse_text(&deep_obj).is_err());
    // A 1 MiB string where a reason goes (over the 2000-byte cap), and in an unknown field.
    let big = "x".repeat(1 << 20);
    let v = {
        let mut v = wire_value();
        v["bundles"][0]["reason"] = big.clone().into();
        v
    };
    assert!(parse(&v).is_err());
    let mut v = wire_value();
    v["note"] = big.into();
    assert!(parse(&v).is_err());
    // A document over the 8 MiB cap.
    let huge = " ".repeat((8 << 20) + 1);
    assert!(parse_text(&format!("{huge}{good}")).is_err());
    // A lone surrogate, high and low.
    for escape in ["\\ud800", "\\udc00", "\\ud800\\u0041"] {
        let text = good.replacen(
            "\"reason\":\"malware\"",
            &format!("\"reason\":\"{escape}\""),
            1,
        );
        assert!(parse_text(&text).is_err(), "{escape}");
    }
    // A valid surrogate pair is fine and round-trips.
    let text = good.replacen("\"reason\":\"malware\"", "\"reason\":\"\\ud83d\\ude00\"", 1);
    let parsed = parse_text(&text).unwrap();
    assert!(parsed.body.bundles.iter().any(|b| b.reason == "\u{1f600}"));
    // Invalid UTF-8 and NUL bytes.
    let mut bytes = good.clone().into_bytes();
    bytes[20] = 0xff;
    assert!(FreshRevocationList::from_wire_json(&bytes).is_err());
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn random_mutations_never_panic_and_a_changed_list_never_verifies() {
    let list = issued();
    let good = list.to_wire_json().unwrap().into_bytes();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..4000 {
        let mut bytes = good.clone();
        for _ in 0..=(rng.next() % 3) {
            let at = usize::try_from(rng.next()).unwrap() % bytes.len();
            bytes[at] = u8::try_from(rng.next() & 0xff).unwrap();
        }
        if let Ok(parsed) = FreshRevocationList::from_wire_json(&bytes)
            && parsed != list
        {
            assert!(verify_at(&parsed, NOW, None).is_err());
        }
    }
    for _ in 0..500 {
        let len = usize::try_from(rng.next() % 300).unwrap();
        let bytes: Vec<u8> = (0..len)
            .map(|_| u8::try_from(rng.next() & 0xff).unwrap())
            .collect();
        let _ = FreshRevocationList::from_wire_json(&bytes);
    }
}
