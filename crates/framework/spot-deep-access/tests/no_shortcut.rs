//! Attacker shortcuts must refuse, including at exact time boundaries.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use ed25519_dalek::Signer;
use spot_deep_access::*;
use spot_iam_access::stepup::AuthMethod;
use spot_signed_command::KeyId;
use spot_support_session::{ActionScope, DocTypeScope, Tier};

#[test]
fn no_receipt() {
    let mut f = Fixture::new();
    let att = f.attestation.clone();
    assert_eq!(
        f.issue_with(None, Some(&att), 100).unwrap_err(),
        DeepError::MissingReceipt
    );
}
#[test]
fn another_request() {
    let mut f = Fixture::new();
    f.receipt.request_hash = [0; 32];
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::WrongRequest);
}
#[test]
fn key_outside_customer_list() {
    let mut f = Fixture::new();
    f.receipt.approver_key = KeyId::from_public_key(&[9; 32]);
    assert_eq!(f.issue().unwrap_err(), DeepError::UnknownApprover);
}
#[test]
fn operator_cannot_approve() {
    let mut f = Fixture::new();
    f.operator_as_customer();
    assert_eq!(f.issue().unwrap_err(), DeepError::OperatorApproval);
}
#[test]
fn expired_receipt() {
    let mut f = Fixture::new();
    f.receipt.valid_to = 100;
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::ExpiredReceipt);
}
#[test]
fn future_receipt() {
    let mut f = Fixture::new();
    f.receipt.valid_from = 101;
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::NotYetValid);
}
#[test]
fn widened_scope() {
    let mut f = Fixture::new();
    f.receipt.approved_scope.doctypes = DocTypeScope::Named(vec!["Payroll".into()]);
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::WidenedScope);
}
#[test]
fn narrowed_scope_cannot_use_original() {
    let mut f = Fixture::new();
    f.receipt.approved_scope.actions = ActionScope::Only(vec!["update".into()]);
    f.sign_receipt();
    let c = f.issue().unwrap();
    let a = f.action();
    let p = f.proof(&c, &a, 1);
    assert_eq!(
        f.use_at(&c, &a, &p, 100).refusal,
        Some(DeepError::OutOfScope)
    );
}
#[test]
fn weak_method_t3() {
    let mut f = Fixture::new();
    f.receipt.method = AuthMethod::PasswordTotp;
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::WeakMethod);
}
#[test]
fn weak_method_t2() {
    let mut f = Fixture::new();
    f.request.spec.tier_ceiling = Tier::T2;
    f.receipt.request_hash = f.request.hash().unwrap();
    f.receipt.method = AuthMethod::Password;
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::WeakMethod);
}
#[test]
fn sso_does_not_claim_second_factor() {
    let mut f = Fixture::new();
    f.request.spec.tier_ceiling = Tier::T2;
    f.receipt.request_hash = f.request.hash().unwrap();
    f.receipt.method = AuthMethod::Sso;
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::WeakMethod);
}
#[test]
fn nonce_replay() {
    let mut f = Fixture::new();
    assert!(f.issue().is_ok());
    assert_eq!(f.issue().unwrap_err(), DeepError::ReplayedReceipt);
}
#[test]
fn attestation_missing() {
    let mut f = Fixture::new();
    let receipt = f.receipt.clone();
    assert_eq!(
        f.issue_with(Some(&receipt), None, 100).unwrap_err(),
        DeepError::MissingAttestation
    );
}
#[test]
fn attestation_malformed() {
    let mut f = Fixture::new();
    f.attestation.signature = vec![1, 2];
    assert_eq!(f.issue().unwrap_err(), DeepError::BadAttestation);
}
#[test]
fn attestation_for_another_key() {
    let mut f = Fixture::new();
    f.attestation.operator_key = f.customer.verifying_key().to_bytes();
    f.attestation.signature = f
        .vendor
        .sign(&f.attestation.signing_bytes().unwrap())
        .to_bytes()
        .to_vec();
    assert_eq!(f.issue().unwrap_err(), DeepError::WrongOperatorKey);
}
#[test]
fn software_attestation() {
    let mut f = Fixture::new();
    f.attestation.kind = "software".into();
    f.attestation.signature = f
        .vendor
        .sign(&f.attestation.signing_bytes().unwrap())
        .to_bytes()
        .to_vec();
    assert_eq!(f.issue().unwrap_err(), DeepError::BadAttestation);
}
#[test]
fn exact_expiry_boundary() {
    let mut f = Fixture::new();
    let c = f.issue().unwrap();
    assert_eq!(c.not_after(), 1900);
    let a = f.action();
    let p = f.proof(&c, &a, 1);
    assert!(f.use_at(&c, &a, &p, 1899).allowed);
    let p = f.proof(&c, &a, 2);
    assert_eq!(f.use_at(&c, &a, &p, 1900).refusal, Some(DeepError::Expired));
}
#[test]
fn immediate_revocation() {
    let mut f = Fixture::new();
    let c = f.issue().unwrap();
    revoke(
        &c,
        "customer-owner",
        "Stop now",
        100,
        &mut f.revocations,
        &mut f.log,
    )
    .unwrap();
    let a = f.action();
    let p = f.proof(&c, &a, 1);
    assert_eq!(f.use_at(&c, &a, &p, 100).refusal, Some(DeepError::Revoked));
}
#[test]
fn counter_replay() {
    let mut f = Fixture::new();
    let c = f.issue().unwrap();
    let a = f.action();
    let p = f.proof(&c, &a, 10);
    assert!(f.use_at(&c, &a, &p, 100).allowed);
    assert_eq!(
        f.use_at(&c, &a, &p, 100).refusal,
        Some(DeepError::ReplayedUse)
    );
}
#[test]
fn different_arguments() {
    let mut f = Fixture::new();
    let c = f.issue().unwrap();
    let mut a = f.action();
    let p = f.proof(&c, &a, 1);
    a.arguments_hash[0] ^= 1;
    assert_eq!(
        f.use_at(&c, &a, &p, 100).refusal,
        Some(DeepError::InvalidProof)
    );
}
#[test]
fn escalate_without_new_receipt() {
    let mut f = Fixture::new();
    let c = f.issue().unwrap();
    assert_eq!(
        f.step(&c, None, 100).unwrap_err(),
        DeepError::MissingReceipt
    );
}
#[test]
fn tampered_customer_signature() {
    let mut f = Fixture::new();
    f.receipt.signature[0] ^= 1;
    assert_eq!(f.issue().unwrap_err(), DeepError::BadSignature);
}
#[test]
fn receipt_cannot_cross_tenants() {
    let mut f = Fixture::new();
    f.request.spec.tenant = "tenant-b".into();
    f.receipt.request_hash = f.request.hash().unwrap();
    f.sign_receipt();
    assert_eq!(f.issue().unwrap_err(), DeepError::AuditUnavailable);
}
#[test]
fn debug_redacts_material() {
    let mut f = Fixture::new();
    let c = f.issue().unwrap();
    let debug = format!("{c:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(&format!("{:?}", [42u8; 32])));
    assert!(!debug.contains(&format!("{:?}", f.operator.verifying_key().to_bytes())));
}
#[test]
fn denial_anomaly_revokes() {
    let mut f = Fixture::new();
    f.policy.max_denials = 2;
    let c = f.issue().unwrap();
    let a = f.action();
    let bad = UseProof {
        counter: 1,
        signature: vec![],
    };
    assert_eq!(
        f.use_at(&c, &a, &bad, 100).refusal,
        Some(DeepError::InvalidProof)
    );
    assert_eq!(
        f.use_at(&c, &a, &bad, 100).refusal,
        Some(DeepError::InvalidProof)
    );
    let p = f.proof(&c, &a, 2);
    assert_eq!(f.use_at(&c, &a, &p, 100).refusal, Some(DeepError::Revoked));
}
#[test]
fn protected_action_requires_fresh_consent() {
    let mut f = Fixture::new();
    f.policy.protected_actions.insert("read".into());
    let c = f.issue().unwrap();
    let a = f.action();
    let p = f.proof(&c, &a, 1);
    assert_eq!(
        f.use_at(&c, &a, &p, 100).refusal,
        Some(DeepError::StepUpRequired)
    );
}
#[test]
fn fresh_action_consent_is_single_use_and_parent_revocation_stops_it() {
    let mut f = Fixture::new();
    f.policy.protected_actions.insert("read".into());
    let c = f.issue().unwrap();
    f.request.id = "step-one".into();
    f.request.spec.expires_at = c.not_after();
    f.receipt.request_hash = f.request.hash().unwrap();
    f.receipt.valid_to = c.not_after();
    f.receipt.nonce = [8; 16];
    f.receipt.approved_scope.actions = ActionScope::Only(vec!["read".into()]);
    f.sign_receipt();
    let receipt = f.receipt.clone();
    let step = f.step(&c, Some(&receipt), 100).unwrap();
    let a = f.action();
    let p = f.proof(&step, &a, 1);
    assert!(f.use_at(&step, &a, &p, 100).allowed);
    let p = f.proof(&step, &a, 2);
    assert_eq!(
        f.use_at(&step, &a, &p, 100).refusal,
        Some(DeepError::StepUpRequired)
    );
    revoke(
        &c,
        "customer-owner",
        "stop parent",
        100,
        &mut f.revocations,
        &mut f.log,
    )
    .unwrap();
    assert_eq!(
        f.use_at(&step, &a, &p, 100).refusal,
        Some(DeepError::Revoked)
    );
}

use proptest::prelude::*;
proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn random_sequences_never_mint_without_valid_consent(ops in prop::collection::vec(0u8..5,1..25)) {
        let mut f=Fixture::new();
        let mut valid_seen=false;
        for (i,op) in ops.into_iter().enumerate() {
            f.receipt.nonce=[i as u8;16]; f.receipt.request_hash=f.request.hash().unwrap(); f.receipt.valid_to=5000; f.receipt.method=AuthMethod::Hardware;
            if op==1 { f.receipt.request_hash=[0;32]; }
            if op==2 { f.receipt.valid_to=99; }
            if op==3 { f.receipt.method=AuthMethod::Password; }
            f.sign_receipt(); let receipt=f.receipt.clone(); let att=f.attestation.clone();
            let presented=if op==4 { None } else { Some(&receipt) };
            if op==0 { valid_seen=true; }
            let result=f.issue_with(presented,Some(&att),100);
            prop_assert_eq!(result.is_ok(),op==0);
            if result.is_ok() { prop_assert!(valid_seen); }
        }
    }
}
