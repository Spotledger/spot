//! The negative control: a stub producer that skips the guard must fail the harness, and the
//! failure names the secret's label and the encoding, never the value.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_field_guard::leakcheck::{Output, assert_no_leak};

/// A producer that serialises the document as it is: it never calls the guard.
fn unguarded_export() -> String {
    let corpus = corpus();
    let mut doc = spot_app_api::Document::new(Name::new(DOCTYPE).unwrap(), RecordId::new_v7());
    doc.set(
        Name::new("card_number").unwrap(),
        Value::Text(corpus.get("card").unwrap().value().to_owned()),
    );
    doc.set(
        Name::new("iban").unwrap(),
        Value::Text(corpus.get("iban").unwrap().value().to_owned()),
    );
    doc.fields()
        .iter()
        .map(|(k, v)| format!("{},{v:?}\n", k.as_str()))
        .collect()
}

#[test]
fn a_producer_that_skips_the_guard_fails_the_harness_without_printing_the_value() {
    let corpus = corpus();
    let output = unguarded_export();
    let result = std::panic::catch_unwind(|| {
        assert_no_leak(&corpus, &[Output::Csv(output)]);
    });
    let payload = result.expect_err("the unguarded producer must fail the harness");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    // It names the label, the encoding and the kind of output...
    assert!(message.contains("card"), "{message}");
    assert!(message.contains("iban"), "{message}");
    assert!(message.contains("raw"), "{message}");
    assert!(message.contains("csv"), "{message}");
    // ...and never the value, in any of the forms the harness searches.
    for secret in &corpus.secrets {
        for part in &secret.planted {
            assert!(
                !message.contains(part.as_str()),
                "the failure printed a value"
            );
        }
    }
}
