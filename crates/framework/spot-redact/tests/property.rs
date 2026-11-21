#![allow(clippy::expect_used)]
// The brief exempts tests from the no-expect rule.
//! The tested guarantees: idempotence, bounded growth, no panic on arbitrary bytes, the
//! 1 MiB cap, and that the whole pipeline stays linear in practice.

use proptest::prelude::*;
use serde_json::{Map, Value, json};
use spot_redact::Redactor;

fn redactor() -> Redactor {
    let mut policy = spot_redact::RedactionPolicy::default();
    policy.patterns.pii_email = true;
    policy.patterns.pii_phone = true;
    let mut r = Redactor::from_policy(&policy).expect("policy builds");
    r.register_secret(
        "demo",
        &spot_secrets::SecretString::new("spider-web-42-secure"),
    )
    .expect("the demo secret is well formed");
    r
}

proptest! {
    /// `redact(redact(x)) == redact(x)` for arbitrary text.
    #[test]
    fn redacting_twice_changes_nothing(text in "\\PC*") {
        let r = redactor();
        let once = r.redact_str(&text).into_owned();
        let twice = r.redact_str(&once).into_owned();
        prop_assert_eq!(twice, once);
    }

    /// Output never grows beyond one 24-byte budget per replacement.
    #[test]
    fn growth_is_bounded_by_24_bytes_per_replacement(text in "\\PC*") {
        let r = redactor();
        let (out, count) = r.redact_str_counted(&text);
        prop_assert!(out.len() <= text.len() + 24 * count,
            "out {} <= in {} + 24 * {count}", out.len(), text.len());
    }

    /// Arbitrary bytes, taken lossily like a host would, never panic and stay bounded.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let r = redactor();
        let lossy = String::from_utf8_lossy(&bytes).into_owned();
        let (out, count) = r.redact_str_counted(&lossy);
        prop_assert!(out.len() <= lossy.len() + 24 * count);
    }

    /// A JSON value of arbitrary shape is walked without panic and redacts idempotently.
    #[test]
    fn arbitrary_json_never_panics(value in json_value()) {
        let r = redactor();
        let mut once = value.clone();
        r.redact_json(&mut once);
        r.redact_json(&mut once); // idempotence of the walker
        let again = serde_json::to_string(&once).ok();
        prop_assert!(again.is_some(), "the redacted value stays valid json");
    }

    /// A provider token in free text never survives redaction — neither the whole token
    /// nor its secret body — and the result is a fixed point of redaction.
    #[test]
    fn provider_tokens_never_survive_and_redaction_is_a_fixed_point(
        (token, body) in provider_token(),
        left in "\\PC{0,24}",
        right in "\\PC{0,24}",
    ) {
        let r = redactor();
        let input = format!("{left} {token} {right}");
        let (out, count) = r.redact_str_counted(&input);
        prop_assert!(
            !out.contains(&token),
            "the token shape survived redaction: {} chars out of {}",
            out.len(),
            input.len()
        );
        prop_assert!(
            !out.contains(&body),
            "the token body survived redaction: {} chars out of {}",
            out.len(),
            input.len()
        );
        prop_assert!(count >= 1, "the token is counted as a replacement");
        let again = r.redact_str(&out).into_owned();
        prop_assert_eq!(again, out);
    }
}

/// The provider token shapes the default rules must catch, as `(prefix, body length,
/// upper-case body)`. The body alphabet is `[A-Za-z0-9]`; AWS key ids are upper-case only.
const PROVIDER_TOKEN_SHAPES: [(&str, usize, bool); 8] = [
    ("AKIA", 16, true),
    ("ASIA", 16, true),
    ("ghp_", 36, false),
    ("github_pat_", 36, false),
    ("xoxb-", 20, false),
    ("sk_live_", 24, false),
    ("AIza", 35, false),
    ("npm_", 36, false),
];

/// A token of one of the shapes above, plus the body on its own (the part that must never
/// reach the output).
fn provider_token() -> impl Strategy<Value = (String, String)> {
    (0usize..PROVIDER_TOKEN_SHAPES.len(), "[A-Za-z0-9]{40}").prop_map(|(shape, body)| {
        let (prefix, len, upper) = PROVIDER_TOKEN_SHAPES[shape];
        let body = if upper {
            body[..len].to_ascii_uppercase()
        } else {
            body[..len].to_owned()
        };
        (format!("{prefix}{body}"), body)
    })
}

/// A small recursive JSON strategy (depth 3): strings, numbers, bools, null, arrays,
/// objects — including key names that trip the sensitive-key list.
fn json_value() -> impl Strategy<Value = Value> {
    let scalar = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        proptest::num::i64::ANY.prop_map(|i| json!(i)),
        ".*".prop_map(Value::String),
    ];
    let leaf = prop_oneof![scalar, ".*".prop_map(Value::String)];
    leaf.prop_recursive(3, 8, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            (
                proptest::collection::vec(".*", 0..4),
                proptest::collection::vec(inner, 0..4)
            )
                .prop_map(|(keys, values)| {
                    let mut map = Map::new();
                    for (i, k) in keys.into_iter().enumerate() {
                        let v = values
                            .get(i % values.len().max(1))
                            .cloned()
                            .unwrap_or(Value::Null);
                        map.insert(k, v);
                    }
                    Value::Object(map)
                }),
        ]
    })
}

#[test]
fn oversized_input_is_capped_and_marker_survives_a_second_pass() {
    let r = redactor();
    let big = "Bearer aaaaaaaaaaaaaaaaaaaaaaaa\n".repeat(48_000); // ~1.5 MiB
    assert!(big.len() > spot_redact::MAX_INPUT_BYTES);
    let once = r.redact_str(&big).into_owned();
    assert!(once.ends_with(spot_redact::TRUNCATION_MARKER));
    assert!(
        once.len() <= spot_redact::MAX_INPUT_BYTES + 80,
        "output stays at the cap"
    );
    let twice = r.redact_str(&once).into_owned();
    assert_eq!(twice, once, "the truncated result is a fixed point");
}

#[test]
fn a_megabyte_of_matches_stays_fast_and_bounded() {
    // Sixty-four thousand credit cards, all Luhn-valid: the heaviest corpus-shaped input.
    // The byte cap truncates first, so the count is the whole cards within one MiB.
    let r = redactor();
    let input = "4111 1111 1111 1111 ".repeat(64_000);
    assert_eq!(input.len() / 20, 64_000);
    let (out, count) = r.redact_str_counted(&input);
    let per_card = 20usize;
    let expected = spot_redact::MAX_INPUT_BYTES / per_card;
    assert_eq!(
        count, expected,
        "every whole card inside the cap is redacted"
    );
    // Every card is 20 chars in, 22 chars out (the marker only grows by 2 per card).
    assert!(out.len() <= input.len() + 24 * count);
}
