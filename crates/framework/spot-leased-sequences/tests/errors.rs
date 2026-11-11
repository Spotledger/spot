//! The `leased_sequences.*` error vocabulary: stable codes, the storage kind each maps onto,
//! the storage-error conversion, and messages that name the series but never the node.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_leased_sequences::{LeaseRefusal, LeasedSequencesError};
use spot_storage_contract::{ErrorKind, StorageError};

fn exhausted() -> LeasedSequencesError {
    LeasedSequencesError::LeaseExhausted {
        series: "INV-2026-".to_owned(),
        requested: 3,
        remaining: 1,
    }
}

#[test]
fn every_variant_has_one_stable_code_and_storage_kind() {
    let cases = [
        (
            exhausted(),
            "leased_sequences.lease_exhausted",
            ErrorKind::Unavailable,
        ),
        (
            LeasedSequencesError::SetNextRefused {
                series: "INV-2026-".to_owned(),
                next: 42,
            },
            "leased_sequences.set_next_refused",
            ErrorKind::Unsupported,
        ),
        (
            LeasedSequencesError::LeaseRefused {
                series: "INV-2026-".to_owned(),
                reason: LeaseRefusal::NotAbove,
            },
            "leased_sequences.lease_refused",
            ErrorKind::InvalidValue,
        ),
        (
            LeasedSequencesError::InvalidCount { count: 0 },
            "leased_sequences.invalid_count",
            ErrorKind::InvalidValue,
        ),
        (
            LeasedSequencesError::Store {
                reason: "disk full".to_owned(),
            },
            "leased_sequences.store",
            ErrorKind::Unavailable,
        ),
    ];

    for (error, code, kind) in cases {
        assert_eq!(error.code(), code);
        assert_eq!(error.kind(), kind, "{code}");
        assert!(!error.to_string().is_empty(), "{code} explains itself");
    }
}

#[test]
fn an_exhausted_lease_converts_to_a_retryable_unavailable_error() {
    let error = StorageError::from(exhausted());
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert!(
        error.kind.is_retryable(),
        "the caller retries once the sync layer renews the lease"
    );
    assert!(error.explanation.contains("INV-2026-"));
    assert!(
        !error.explanation.contains("branch_07"),
        "no node identity leaks"
    );
}

#[test]
fn a_refused_set_next_is_unsupported_not_retryable() {
    let error = StorageError::from(LeasedSequencesError::SetNextRefused {
        series: "INV-2026-".to_owned(),
        next: 42,
    });
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(!error.kind.is_retryable());
    assert!(error.explanation.contains("INV-2026-"));
}

#[test]
fn a_zero_count_is_an_invalid_value() {
    let error = StorageError::from(LeasedSequencesError::InvalidCount { count: 0 });
    assert_eq!(error.kind, ErrorKind::InvalidValue);
    assert!(error.explanation.contains('0'));
}

#[test]
fn a_store_failure_is_unavailable_and_explains_why() {
    let error = StorageError::from(LeasedSequencesError::Store {
        reason: "the lease store is read-only".to_owned(),
    });
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert!(error.explanation.contains("read-only"));
}
