//! A property run over a generated corpus: no plaintext of any personal value appears anywhere
//! in the serialised protected image.
//!
//! The corpus is deterministic (a xorshift stream, no dependency), so the test is a property
//! run over 64 generated records rather than a single worked example. `proptest` is not one of
//! the crate's allowed dependencies, so no `[dev-dependencies]` are added.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use super::{at, image, keys, set_at, spec};
use crate::{CounterNonces, protect, reveal};

/// A xorshift64 step, so the corpus is generated but reproducible.
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// True when `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[test]
fn no_personal_value_appears_anywhere_in_the_protected_image() {
    let spec = spec();
    let keys = keys();
    let mut state = 0x5eed_1234_abcd_9876u64;
    let mut cases = 0u32;

    for case in 0..64u32 {
        let token = next(&mut state);
        let email = format!("pii-{case}-{token:016x}@example.test");
        let phone = format!("pii-{case}-{token:016x}-phone");
        let first_note = format!("pii-{case}-{token:016x}-note-a");
        let second_note = format!("pii-{case}-{token:016x}-note-b");
        let guardian = format!("pii-{case}-{token:016x}-guardian");
        let values = [
            email.clone(),
            phone.clone(),
            first_note.clone(),
            second_note.clone(),
            guardian.clone(),
        ];

        let mut record = image("c-1", "alice", "bob", 1 + u64::from(case));
        set_at(&mut record, "email", json!(email));
        set_at(&mut record, "phone", json!(phone));
        set_at(&mut record, "items.0.note", json!(first_note));
        set_at(&mut record, "items.1.note", json!(second_note));
        set_at(&mut record, "guardian_email", json!(guardian));

        let protected = protect(&record, &spec, &keys, &mut CounterNonces::new()).expect("protect");
        let serialised = serde_json::to_string(&protected).expect("serialise");
        for value in &values {
            assert!(
                !serialised.contains(value.as_str()),
                "case {case}: the plaintext {value} is in the protected image"
            );
        }

        // Not inside the ciphertext either: base64 of the sealed bytes is what is stored, and
        // the plaintext must not appear in it.
        let ciphertext = URL_SAFE_NO_PAD
            .decode(at(&protected, "email")["ct"].as_str().expect("ct"))
            .expect("base64");
        assert!(
            !contains(&ciphertext, values[0].as_bytes()),
            "case {case}: the plaintext is inside the ciphertext bytes"
        );

        assert_eq!(reveal(&protected, &spec, &keys).expect("reveal"), record);
        cases += 1;
    }

    assert_eq!(cases, 64, "the whole corpus ran");
}

#[test]
fn a_personal_value_may_be_any_json() {
    let spec = spec();
    let keys = keys();
    let values: [Value; 5] = [
        json!(1970),
        Value::Null,
        json!([1, "two", {"three": 3}]),
        json!({"consent": true}),
        json!(""),
    ];

    for value in values {
        let mut record = image("c-1", "alice", "bob", 1);
        set_at(&mut record, "email", value.clone());
        let protected = protect(&record, &spec, &keys, &mut CounterNonces::new()).expect("protect");
        assert_ne!(at(&protected, "email"), &value);
        assert_eq!(
            reveal(&protected, &spec, &keys).expect("reveal")["email"],
            value
        );
    }
}

#[test]
fn the_subject_id_is_the_documented_cleartext() {
    // An envelope names its subject in the clear: reveal has to find the key without opening
    // the value, and an erasure index has to know who a record belongs to. Everything else
    // about the value is sealed, which the test above pins.
    let keys = keys();
    let protected = protect(
        &image("c-1", "alice", "bob", 1),
        &spec(),
        &keys,
        &mut CounterNonces::new(),
    )
    .expect("protect");
    let serialised = serde_json::to_string(&protected).expect("serialise");
    assert!(serialised.contains("\"alice\""));
    assert!(!serialised.contains("ada@example.test"));
}
