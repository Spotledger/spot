//! The consent wire form and the tenant-side check (WP-SEC-07.T2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::Fixture;
use ed25519_dalek::{Signer, SigningKey};
use spot_deep_access::{ConsentRecord, DeepError, check_consent, to_hex};

fn record(f: &Fixture) -> ConsentRecord {
    ConsentRecord {
        request_hash: to_hex(&f.receipt.request_hash),
        approver: f.receipt.approver.clone(),
        approver_public_key: to_hex(&f.customer.verifying_key().to_bytes()),
        method: f.receipt.method,
        approved_scope: f.receipt.approved_scope.clone(),
        valid_from: f.receipt.valid_from,
        valid_to: f.receipt.valid_to,
        nonce: to_hex(&f.receipt.nonce),
    }
}

#[test]
fn a_record_and_signature_make_the_receipt_the_customer_signed() {
    let f = Fixture::new();
    let text = serde_json::to_string(&record(&f)).unwrap();
    let parsed = ConsentRecord::from_json(&text).unwrap();
    let receipt = parsed.into_receipt(f.receipt.signature.clone()).unwrap();
    assert_eq!(receipt.hash().unwrap(), f.receipt.hash().unwrap());
    assert!(check_consent(&f.request, &receipt, &f.trust, None, 200).is_ok());
}

#[test]
fn a_consent_signed_by_the_wrong_key_is_refused() {
    let f = Fixture::new();
    let mut receipt = record(&f).into_receipt(vec![]).unwrap();
    let wrong = SigningKey::from_bytes(&[9; 32]);
    receipt.signature = wrong
        .sign(&receipt.signing_bytes().unwrap())
        .to_bytes()
        .to_vec();
    assert_eq!(
        check_consent(&f.request, &receipt, &f.trust, None, 200),
        Err(DeepError::BadSignature)
    );
}

#[test]
fn a_key_that_is_not_enrolled_is_an_unknown_approver() {
    let f = Fixture::new();
    let stranger = SigningKey::from_bytes(&[9; 32]);
    let mut rec = record(&f);
    rec.approver_public_key = to_hex(&stranger.verifying_key().to_bytes());
    let mut receipt = rec.into_receipt(vec![]).unwrap();
    receipt.signature = stranger
        .sign(&receipt.signing_bytes().unwrap())
        .to_bytes()
        .to_vec();
    assert_eq!(
        check_consent(&f.request, &receipt, &f.trust, None, 200),
        Err(DeepError::UnknownApprover)
    );
}

#[test]
fn a_consent_for_another_request_an_expired_one_and_a_wider_scope_are_refused() {
    let f = Fixture::new();
    // Another request.
    let mut other = f.request.clone();
    other.reason = "A different reason".into();
    other.spec.reason = "A different reason".into();
    assert_eq!(
        check_consent(&other, &f.receipt, &f.trust, None, 200),
        Err(DeepError::WrongRequest)
    );
    // Expired and not yet valid.
    assert_eq!(
        check_consent(&f.request, &f.receipt, &f.trust, None, 5000),
        Err(DeepError::ExpiredReceipt)
    );
    assert_eq!(
        check_consent(&f.request, &f.receipt, &f.trust, None, 50),
        Err(DeepError::NotYetValid)
    );
    // A wider scope than requested, re-signed by the real customer key.
    let mut rec = record(&f);
    rec.approved_scope.doctypes = spot_support_session::DocTypeScope::AllRead;
    let mut receipt = rec.into_receipt(vec![]).unwrap();
    receipt.signature = f
        .customer
        .sign(&receipt.signing_bytes().unwrap())
        .to_bytes()
        .to_vec();
    assert_eq!(
        check_consent(&f.request, &receipt, &f.trust, None, 200),
        Err(DeepError::WidenedScope)
    );
}

#[test]
fn the_operator_cannot_approve_its_own_request() {
    let f = Fixture::new();
    // The consent names the requesting operator as approver.
    let mut rec = record(&f);
    rec.approver = f.request.spec.requested_by.clone();
    let receipt = rec.into_receipt(vec![0; 64]).unwrap();
    assert!(check_consent(&f.request, &receipt, &f.trust, None, 200).is_err());
    // The customer's key equal to the operator's key is refused too.
    let key = f.customer.verifying_key().to_bytes();
    assert_eq!(
        check_consent(&f.request, &f.receipt, &f.trust, Some(&key), 200),
        Err(DeepError::OperatorApproval)
    );
}

#[test]
fn malformed_records_are_invalid_requests() {
    assert_eq!(
        ConsentRecord::from_json("{\"nope\":1}"),
        Err(DeepError::InvalidRequest)
    );
    assert_eq!(
        ConsentRecord::from_json(&"x".repeat(9000)),
        Err(DeepError::InvalidRequest)
    );
    let f = Fixture::new();
    let mut rec = record(&f);
    rec.nonce = "zz".repeat(16);
    assert_eq!(
        rec.into_receipt(vec![]).err(),
        Some(DeepError::InvalidRequest)
    );
    let mut rec = record(&f);
    rec.request_hash = "ab".to_owned();
    assert_eq!(
        rec.into_receipt(vec![]).err(),
        Some(DeepError::InvalidRequest)
    );
}
