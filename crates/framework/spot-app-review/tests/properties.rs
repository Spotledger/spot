//! The property test: arbitrary bytes never panic and always produce a report.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_app_review::{NullProbe, ReviewConfig, Verdict, review};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn arbitrary_bytes_never_panic_and_always_report(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let report = review(&bytes, &ReviewConfig::default(), &NullProbe);
        prop_assert_eq!(report.counts.total() as usize, report.findings.len());
    }

    #[test]
    fn bytes_that_do_not_open_are_rejected_with_findings(
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let report = review(&bytes, &ReviewConfig::default(), &NullProbe);
        if report.digest.is_none() {
            prop_assert_eq!(report.verdict, Verdict::Rejected);
            prop_assert!(!report.findings.is_empty());
        }
    }
}

#[test]
fn empty_bytes_are_rejected_not_a_panic() {
    let report = review(&[], &ReviewConfig::default(), &NullProbe);
    assert_eq!(report.verdict, Verdict::Rejected);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.rule == "bundle.bad_archive")
    );
}
