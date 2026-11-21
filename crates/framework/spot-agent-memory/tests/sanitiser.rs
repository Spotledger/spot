//! The no-business-data rule from outside the crate: what `Sanitiser::check` accepts, redacts and
//! refuses, and the promise that a refusal never echoes the value it caught.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_agent_memory::{FieldClass, MAX_CONTENT_BYTES, RecordRef, Sanitiser, SanitiserError};

fn sanitiser() -> Sanitiser {
    Sanitiser::standard().expect("the standard sanitiser compiles")
}

/// One row of the matrix: the content, and what the check must do with it.
enum Expected {
    /// The text passes untouched.
    Clean,
    /// The text is redacted and accepted; the value must not survive.
    Redacted(&'static str),
    /// The text is refused, with this class.
    Refused(FieldClass),
}

#[test]
fn the_sanitiser_matrix() {
    let sanitiser = sanitiser();
    let rows: Vec<(&str, Expected)> = vec![
        // Record references and diagnosis prose pass.
        (
            "job:018f-0001 stuck in running after 3 retries",
            Expected::Clean,
        ),
        (
            "the operator retried the job and it finished",
            Expected::Clean,
        ),
        (
            "backend.postgres_version 16.4 on the standalone node",
            Expected::Clean,
        ),
        // Secrets: refused, never merely redacted, because a secret has no place in memory.
        (
            "password=hunter2hunter2",
            Expected::Refused(FieldClass::Secret),
        ),
        (
            "the key AKIAIOSFODNN7EXAMPLE was in the log",
            Expected::Refused(FieldClass::Secret),
        ),
        (
            "token: abcdefghijklmnop",
            Expected::Refused(FieldClass::Secret),
        ),
        (
            "postgres://user:hunter2@db:5432/site",
            Expected::Refused(FieldClass::Secret),
        ),
        // Financial values: amounts, card numbers and IBANs.
        (
            "the invoice total was Rs 12,500",
            Expected::Refused(FieldClass::Financial),
        ),
        ("amount: 1500", Expected::Refused(FieldClass::Financial)),
        (
            "unit_price = 99.50",
            Expected::Refused(FieldClass::Financial),
        ),
        (
            "a payment of 1,234,567 was posted",
            Expected::Refused(FieldClass::Financial),
        ),
        (
            "card 4111 1111 1111 1111",
            Expected::Refused(FieldClass::Financial),
        ),
        (
            "IBAN DE89370400440532013000",
            Expected::Refused(FieldClass::Financial),
        ),
        ("qty: 12", Expected::Refused(FieldClass::Financial)),
        // Personal data: a field-name assignment is refused, a bare address is redacted.
        ("customer: Acme Trading", Expected::Refused(FieldClass::Pii)),
        (
            "email = bob@example.com",
            Expected::Refused(FieldClass::Pii),
        ),
        (
            "the operator bob@example.com retried the job",
            Expected::Redacted("bob@example.com"),
        ),
        (
            "call +44 20 7946 0958 about the retry",
            Expected::Redacted("7946"),
        ),
    ];
    for (content, expected) in rows {
        match (sanitiser.check(content), expected) {
            (Ok(summary), Expected::Clean) => {
                assert_eq!(summary.as_str(), content, "`{content}` passes untouched");
                assert!(!summary.was_redacted());
            }
            (Ok(summary), Expected::Redacted(value)) => {
                assert!(
                    !summary.as_str().contains(value),
                    "`{content}` loses `{value}`"
                );
                assert!(summary.was_redacted(), "`{content}` reports a redaction");
            }
            (Err(error), Expected::Refused(class)) => {
                assert_eq!(
                    error.class(),
                    Some(class),
                    "`{content}` is refused as {class}"
                );
            }
            (result, _) => panic!("`{content}` gave the wrong verdict: {result:?}"),
        }
    }
}

#[test]
fn a_documents_contents_never_enter_memory() {
    let sanitiser = sanitiser();
    // A rendered invoice: an identity, an amount, a tax amount and a bank account.
    let invoice = "Sales Invoice SINV-2026-0001\n\
                   customer: Acme Trading LLC\n\
                   net_total: 1000.00\n\
                   tax: 170.00\n\
                   total: 1170.00\n\
                   IBAN DE89370400440532013000";
    let error = sanitiser
        .check(invoice)
        .expect_err("an invoice is not memory text");
    assert!(matches!(
        error,
        SanitiserError::FieldValue { .. } | SanitiserError::Amount
    ));

    // A document's bytes (a PDF header and control characters) are not prose either.
    assert_eq!(
        sanitiser.check("%PDF-1.7\n\u{1}\u{2}\u{3}").unwrap_err(),
        SanitiserError::ControlCharacters
    );

    // A whole record dump: over the bound, so it cannot be stored even if it were clean.
    let dump = "note ".repeat(400);
    match sanitiser.check(&dump).unwrap_err() {
        SanitiserError::TooLong { bytes, max } => {
            assert!(bytes > MAX_CONTENT_BYTES);
            assert_eq!(max, MAX_CONTENT_BYTES);
        }
        other => panic!("a long dump is refused for its size, not {other:?}"),
    }
}

#[test]
fn a_refusal_names_the_class_and_never_the_value() {
    let sanitiser = sanitiser();
    for (content, value) in [
        ("password=hunter2hunter2", "hunter2"),
        ("the invoice total was Rs 12,500", "12,500"),
        ("customer: Acme Trading LLC", "Acme Trading LLC"),
    ] {
        let error = sanitiser.check(content).unwrap_err();
        let text = error.to_string();
        assert!(!text.contains(value), "`{text}` quotes no value");
        assert!(
            text.contains("may reference records only by type and id"),
            "the message states the rule: `{text}`"
        );
    }
}

#[test]
fn a_record_reference_is_the_only_way_memory_names_a_record() {
    let reference = RecordRef::new("sales_invoice", "018f-0001").expect("a well-formed reference");
    let sanitiser = sanitiser();
    let summary = sanitiser
        .check(&format!("posted {reference} after the retry"))
        .expect("a reference is memory text");
    assert!(summary.as_str().contains("sales_invoice:018f-0001"));
    // And the reference itself cannot carry a value: it is a type and an id.
    assert!(RecordRef::new("sales_invoice", "1000.00 total").is_err());
}

#[test]
fn the_scan_is_idempotent() {
    let sanitiser = sanitiser();
    let first = sanitiser
        .check("mail bob@example.com about the retry")
        .unwrap();
    let second = sanitiser.check(first.as_str()).unwrap();
    assert_eq!(second.as_str(), first.as_str());
    assert!(!second.was_redacted());
}
