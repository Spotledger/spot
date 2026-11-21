//! The financial fingerprint compare: exact decimals, no floats, and every name that differs.

use crate::tests::amounts;
use crate::{
    Amount, Fingerprint, FingerprintDifferenceKind, ValueDifferenceKind, compare_fingerprints,
};

/// A fingerprint from a name and exact decimal strings.
fn fingerprint(name: &str, entries: &[(&str, &str)]) -> Fingerprint {
    Fingerprint::new(name, amounts(entries)).expect("a valid fingerprint")
}

#[test]
fn equal_fingerprints_match() {
    let live = vec![fingerprint(
        "trial_balance",
        &[("4000/ACME", "1000.00"), ("4010/ACME", "-250.5")],
    )];
    let report = compare_fingerprints(&live, &live).expect("two sides");
    assert!(report.is_match());
    assert!(report.names().is_empty());

    // The same amount written differently is the same amount.
    let rebuilt = vec![fingerprint(
        "trial_balance",
        &[("4000/ACME", "0001000.000"), ("4010/ACME", "-250.50")],
    )];
    assert!(
        compare_fingerprints(&live, &rebuilt)
            .expect("two sides")
            .is_match()
    );
}

#[test]
fn a_changed_amount_is_reported_with_both_sides() {
    let live = vec![fingerprint("trial_balance", &[("4000/ACME", "1000.00")])];
    let rebuilt = vec![fingerprint("trial_balance", &[("4000/ACME", "1000.01")])];
    let report = compare_fingerprints(&live, &rebuilt).expect("two sides");

    assert_eq!(report.names(), vec!["trial_balance"]);
    let difference = &report.differing[0];
    assert_eq!(difference.kind, FingerprintDifferenceKind::ValuesDiffer);
    let value = &difference.values[0];
    assert_eq!(value.key, "4000/ACME");
    assert_eq!(value.kind, ValueDifferenceKind::ValueDiffers);
    assert_eq!(
        value.live.as_ref().map(Amount::to_canonical_string),
        Some("1000".to_owned())
    );
    assert_eq!(
        value.rebuilt.as_ref().map(Amount::to_canonical_string),
        Some("1000.01".to_owned())
    );
}

#[test]
fn a_value_on_one_side_is_reported_and_named() {
    let live = vec![fingerprint(
        "stock_balance",
        &[
            ("ITEM-1/WH-1/standard", "12"),
            ("ITEM-2/WH-1/standard", "4"),
        ],
    )];
    let rebuilt = vec![fingerprint(
        "stock_balance",
        &[("ITEM-1/WH-1/standard", "12")],
    )];

    let report = compare_fingerprints(&live, &rebuilt).expect("two sides");
    let value = &report.differing[0].values[0];
    assert_eq!(value.kind, ValueDifferenceKind::MissingInRebuild);
    assert_eq!(value.key, "ITEM-2/WH-1/standard");
    assert_eq!(value.rebuilt, None);

    let report = compare_fingerprints(&rebuilt, &live).expect("two sides");
    let value = &report.differing[0].values[0];
    assert_eq!(value.kind, ValueDifferenceKind::ExtraInRebuild);
    assert_eq!(value.live, None);

    // Every differing name is listed, in name order.
    let both = vec![
        fingerprint("trial_balance", &[("4000/ACME", "1")]),
        fingerprint("ar_ageing", &[("0-30", "2")]),
    ];
    let report = compare_fingerprints(&both, &both[..1]).expect("two sides");
    assert_eq!(report.names(), vec!["ar_ageing"]);
}

#[test]
fn a_whole_fingerprint_missing_from_a_side_is_named() {
    let live = vec![fingerprint("ap_ageing", &[("0-30", "10")])];
    let report = compare_fingerprints(&live, &[]).expect("two sides");
    assert_eq!(report.names(), vec!["ap_ageing"]);
    assert_eq!(
        report.differing[0].kind,
        FingerprintDifferenceKind::MissingInRebuild
    );
    assert!(report.differing[0].values.is_empty());

    let report = compare_fingerprints(&[], &live).expect("two sides");
    assert_eq!(
        report.differing[0].kind,
        FingerprintDifferenceKind::ExtraInRebuild
    );
}

#[test]
fn exact_decimals_are_compared_exactly() {
    // The one-unit-in-the-eighth-decimal difference a float would lose.
    let live = vec![fingerprint("trial_balance", &[("4000/ACME", "0.01")])];
    let rebuilt = vec![fingerprint(
        "trial_balance",
        &[("4000/ACME", "0.0100000001")],
    )];
    let report = compare_fingerprints(&live, &rebuilt).expect("two sides");
    assert_eq!(
        report.differing[0].values[0].kind,
        ValueDifferenceKind::ValueDiffers
    );

    // One canonical form per amount.
    for (text, canonical) in [
        ("00012.3400", "12.34"),
        ("+0.5", "0.5"),
        ("-0.0", "0"),
        ("0.001", "0.001"),
    ] {
        assert_eq!(
            Amount::decimal(text)
                .expect("a decimal")
                .to_canonical_string(),
            canonical,
            "{text}"
        );
    }
    assert_eq!(Amount::minor(-5).to_canonical_string(), "-5");
    assert!(Amount::decimal("-0.000").expect("a decimal").is_zero());
    assert!(Amount::minor(0).is_zero());

    // Anything that is not an exact decimal is refused, never parsed as a float.
    for text in [
        "", "-", ".", ".5", "5.", "1.2.3", "1e3", "1E3", "1,000", " 1", "1 ", "0x10",
    ] {
        let error = Amount::decimal(text).expect_err(text);
        assert_eq!(error.code(), "journal_audit.amount_invalid", "{text}");
    }
}

#[test]
fn minor_units_and_decimals_are_not_converted() {
    // A minor unit has no currency and so no implied exponent: 1000 is not 10.00.
    let live = vec![
        Fingerprint::new(
            "trial_balance",
            [("4000/ACME".to_owned(), Amount::minor(1000))],
        )
        .expect("a fingerprint"),
    ];
    let rebuilt = vec![fingerprint("trial_balance", &[("4000/ACME", "10.00")])];
    let report = compare_fingerprints(&live, &rebuilt).expect("two sides");
    assert_eq!(
        report.differing[0].values[0].kind,
        ValueDifferenceKind::ValueDiffers
    );
}

#[test]
fn a_fingerprint_that_cannot_be_compared_is_refused() {
    let repeated_key = Fingerprint::new(
        "trial_balance",
        [
            ("4000/ACME".to_owned(), Amount::minor(1)),
            ("4000/ACME".to_owned(), Amount::minor(2)),
        ],
    )
    .expect_err("a repeated key");
    assert_eq!(repeated_key.code(), "journal_audit.fingerprint_invalid");
    assert!(repeated_key.to_string().contains("a key repeats"));

    let unnamed = Fingerprint::new("  ", [("4000/ACME".to_owned(), Amount::minor(1))])
        .expect_err("an unnamed fingerprint");
    assert_eq!(unnamed.code(), "journal_audit.fingerprint_invalid");

    let empty_key = Fingerprint::new("trial_balance", [(String::new(), Amount::minor(1))])
        .expect_err("an empty key");
    assert_eq!(empty_key.code(), "journal_audit.fingerprint_invalid");

    // Two fingerprints of one side with the same name cannot be told apart.
    let twice = vec![
        fingerprint("trial_balance", &[("4000/ACME", "1")]),
        fingerprint("trial_balance", &[("4000/ACME", "1")]),
    ];
    let error = compare_fingerprints(&twice, &[]).expect_err("a repeated name");
    assert_eq!(error.code(), "journal_audit.fingerprint_invalid");
    assert!(error.to_string().contains("trial_balance"));
}
