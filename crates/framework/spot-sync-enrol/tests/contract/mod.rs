//! The store contract: the scenarios both the in-memory and the PostgreSQL store must pass,
//! run through the service so the whole path (code, hash, store, directory) is exercised.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;

use spot_sync_enrol::{
    EnrolError, EnrolmentStore, NodeIdentity, NodeStatus, Severity, fingerprint_of,
};
use spot_sync_http::{ClientCertificate, NodeDirectory};

use crate::common::TestClock;
use crate::common::{Rig, T0, cert_between, identity, node, request, rig};

pub fn client_cert(identity: &NodeIdentity) -> ClientCertificate {
    ClientCertificate::from_der(identity.certificate_der().to_vec())
}

/// A rig over a fresh store from `make`.
pub fn fresh(make: &dyn Fn() -> Arc<dyn EnrolmentStore>) -> Rig {
    rig(make(), TestClock::new())
}

pub fn run_all(make: &dyn Fn() -> Arc<dyn EnrolmentStore>) {
    enrol_redeem_authenticate(&fresh(make));
    every_wrong_code_is_the_same_error(&fresh(make));
    a_used_code_is_dead(&fresh(make));
    no_downgrade_and_tombstones(&fresh(make));
    lockout(&fresh(make));
    rotation_with_overlap(&fresh(make));
    revoke_and_suspend_are_immediate(&fresh(make));
    expiring_conditions(&fresh(make));
    the_directory_fails_closed(&fresh(make));
}

fn enrol_redeem_authenticate(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_1"))
        .unwrap();
    let id = identity("edge_1");
    // Before redemption the certificate is unknown.
    assert!(rig.directory.resolve(&client_cert(&id)).is_err());
    let redeemed = rig
        .service
        .redeem(
            &issued.code,
            &node("edge_1"),
            id.certificate_der(),
            "10.0.0.1",
        )
        .unwrap();
    assert_eq!(redeemed.fingerprint, id.fingerprint());
    assert_eq!(redeemed.partitions, vec!["branch_01".to_owned()]);
    let authenticated = rig.directory.resolve(&client_cert(&id)).unwrap();
    assert_eq!(authenticated.node, node("edge_1"));
    assert!(authenticated.partitions.contains("branch_01"));
    let listed = rig.service.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, NodeStatus::Active);
    assert_eq!(
        listed[0].fingerprint.as_deref(),
        Some(id.fingerprint().as_str())
    );
}

/// Unknown, expired, used, malformed and wrong-node codes are one error with one text.
fn every_wrong_code_is_the_same_error(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment(
            "admin",
            &spot_sync_enrol::EnrolRequest {
                ttl_seconds: Some(60),
                ..request("edge_2")
            },
        )
        .unwrap();
    let id = identity("edge_2");
    let mut seen = Vec::new();
    // Unknown id.
    seen.push(rig.service.redeem(
        "nope.00112233445566778899aabbccddeeff",
        &node("edge_2"),
        id.certificate_der(),
        "s1",
    ));
    // Right id, wrong secret.
    let wrong = format!("{}.{}", issued.id, "ff".repeat(16));
    seen.push(
        rig.service
            .redeem(&wrong, &node("edge_2"), id.certificate_der(), "s2"),
    );
    // Malformed.
    seen.push(
        rig.service
            .redeem("garbage", &node("edge_2"), id.certificate_der(), "s3"),
    );
    // Right code, wrong node.
    seen.push(rig.service.redeem(
        &issued.code,
        &node("other_node"),
        id.certificate_der(),
        "s4",
    ));
    // Expired.
    rig.clock.advance(61);
    seen.push(
        rig.service
            .redeem(&issued.code, &node("edge_2"), id.certificate_der(), "s5"),
    );
    let messages: Vec<String> = seen
        .iter()
        .map(|r| match r {
            Err(e) => {
                assert_eq!(e.code(), "sync_enrol.code_invalid");
                e.to_string()
            }
            Ok(_) => panic!("a wrong code was accepted"),
        })
        .collect();
    assert!(messages.windows(2).all(|w| w[0] == w[1]), "{messages:?}");
    for message in &messages {
        assert!(!message.contains(&issued.code), "the code leaked");
    }
}

fn a_used_code_is_dead(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_3"))
        .unwrap();
    let id = identity("edge_3");
    rig.service
        .redeem(&issued.code, &node("edge_3"), id.certificate_der(), "s")
        .unwrap();
    let again = rig
        .service
        .redeem(
            &issued.code,
            &node("edge_3"),
            identity("edge_3").certificate_der(),
            "s",
        )
        .unwrap_err();
    assert_eq!(again.code(), "sync_enrol.code_invalid");
}

fn no_downgrade_and_tombstones(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_4"))
        .unwrap();
    let id = identity("edge_4");
    rig.service
        .redeem(&issued.code, &node("edge_4"), id.certificate_der(), "s")
        .unwrap();
    // No re-enrolment over an active identity.
    assert_eq!(
        rig.service
            .create_enrolment("admin", &request("edge_4"))
            .unwrap_err(),
        EnrolError::NodeExists
    );
    rig.service.revoke("admin", &node("edge_4")).unwrap();
    // After revocation the node can be enrolled again, but never with the revoked certificate.
    let second = rig
        .service
        .create_enrolment("admin", &request("edge_4"))
        .unwrap();
    assert_eq!(
        rig.service
            .redeem(&second.code, &node("edge_4"), id.certificate_der(), "s")
            .unwrap_err(),
        EnrolError::CertRevoked
    );
    // A refusal after the code was good did not consume it.
    let fresh_id = identity("edge_4");
    rig.service
        .redeem(
            &second.code,
            &node("edge_4"),
            fresh_id.certificate_der(),
            "s",
        )
        .unwrap();
    assert!(rig.directory.resolve(&client_cert(&fresh_id)).is_ok());
    let old = rig.directory.resolve(&client_cert(&id)).unwrap_err();
    assert_eq!(old.code, "sync_http.identity_revoked");
}

fn lockout(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_5"))
        .unwrap();
    let id = identity("edge_5");
    let wrong = format!("{}.{}", issued.id, "aa".repeat(16));
    for _ in 0..5 {
        assert_eq!(
            rig.service
                .redeem(
                    &wrong,
                    &node("edge_5"),
                    id.certificate_der(),
                    "198.51.100.7"
                )
                .unwrap_err(),
            EnrolError::CodeInvalid
        );
    }
    // Now even the correct code from that source is refused, without any hint.
    assert_eq!(
        rig.service
            .redeem(
                &issued.code,
                &node("edge_5"),
                id.certificate_der(),
                "198.51.100.7"
            )
            .unwrap_err(),
        EnrolError::LockedOut
    );
    // Another source is not affected.
    let mid = rig.service.redeem(
        &issued.code,
        &node("edge_5"),
        id.certificate_der(),
        "203.0.113.9",
    );
    assert!(mid.is_ok(), "{mid:?}");
    // After the lockout the first source may try again (the code is used, so it is just wrong).
    rig.clock.advance(3_601);
    assert_eq!(
        rig.service
            .redeem(
                &issued.code,
                &node("edge_5"),
                id.certificate_der(),
                "198.51.100.7"
            )
            .unwrap_err(),
        EnrolError::CodeInvalid
    );
}

fn rotation_with_overlap(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_6"))
        .unwrap();
    let old = identity("edge_6");
    rig.service
        .redeem(&issued.code, &node("edge_6"), old.certificate_der(), "s")
        .unwrap();
    let new = identity("edge_6");
    let now = rig.clock.0.load(std::sync::atomic::Ordering::SeqCst);
    // A proof made with the wrong key is refused.
    let stranger = identity("edge_6");
    let bad = stranger
        .prove_rotation(&node("edge_6"), &new.fingerprint(), now)
        .unwrap();
    assert_eq!(
        rig.service
            .rotate(&node("edge_6"), new.certificate_der(), &bad, now, 3_600)
            .unwrap_err(),
        EnrolError::ProofInvalid
    );
    // A proof for another fingerprint is refused too.
    let other = old
        .prove_rotation(&node("edge_6"), "deadbeef", now)
        .unwrap();
    assert_eq!(
        rig.service
            .rotate(&node("edge_6"), new.certificate_der(), &other, now, 3_600)
            .unwrap_err(),
        EnrolError::ProofInvalid
    );
    let proof = old
        .prove_rotation(&node("edge_6"), &new.fingerprint(), now)
        .unwrap();
    rig.service
        .rotate(&node("edge_6"), new.certificate_der(), &proof, now, 3_600)
        .unwrap();
    // Both certificates authenticate during the overlap.
    rig.directory.refresh().unwrap();
    assert!(rig.directory.resolve(&client_cert(&old)).is_ok());
    assert!(rig.directory.resolve(&client_cert(&new)).is_ok());
    // A replayed rotation (the same new certificate) is refused: it is pinned already.
    assert_eq!(
        rig.service
            .rotate(&node("edge_6"), new.certificate_der(), &proof, now, 3_600)
            .unwrap_err()
            .code(),
        "sync_enrol.proof_invalid",
        "the proof was made with the old key, which is no longer the active one"
    );
    // After the overlap the old certificate is dropped automatically.
    rig.clock.advance(3_601);
    rig.directory.refresh().unwrap();
    assert!(rig.directory.resolve(&client_cert(&old)).is_err());
    assert!(rig.directory.resolve(&client_cert(&new)).is_ok());
    // The overlap is capped at 30 days whatever is asked.
    let newer = identity("edge_6");
    let now = rig.clock.0.load(std::sync::atomic::Ordering::SeqCst);
    let proof = new
        .prove_rotation(&node("edge_6"), &newer.fingerprint(), now)
        .unwrap();
    rig.service
        .rotate(
            &node("edge_6"),
            newer.certificate_der(),
            &proof,
            now,
            365 * 86_400,
        )
        .unwrap();
    rig.clock.advance(31 * 86_400);
    rig.directory.refresh().unwrap();
    assert!(
        rig.directory.resolve(&client_cert(&new)).is_err(),
        "the cap held"
    );
    assert!(rig.directory.resolve(&client_cert(&newer)).is_ok());
}

fn revoke_and_suspend_are_immediate(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_7"))
        .unwrap();
    let id = identity("edge_7");
    rig.service
        .redeem(&issued.code, &node("edge_7"), id.certificate_der(), "s")
        .unwrap();
    rig.service.suspend("admin", &node("edge_7")).unwrap();
    assert_eq!(
        rig.directory.resolve(&client_cert(&id)).unwrap_err().code,
        "sync_http.identity_revoked"
    );
    rig.service.resume("admin", &node("edge_7")).unwrap();
    assert!(rig.directory.resolve(&client_cert(&id)).is_ok());
    // Revocation: no refresh needed, and a refresh agrees.
    rig.service.revoke("admin", &node("edge_7")).unwrap();
    assert_eq!(
        rig.directory.resolve(&client_cert(&id)).unwrap_err().code,
        "sync_http.identity_revoked"
    );
    rig.directory.refresh().unwrap();
    assert_eq!(
        rig.directory.resolve(&client_cert(&id)).unwrap_err().code,
        "sync_http.identity_revoked"
    );
    // Final: neither resume nor suspend nor rotate touches a revoked node.
    assert_eq!(
        rig.service.resume("admin", &node("edge_7")).unwrap_err(),
        EnrolError::NodeRevoked
    );
    assert_eq!(
        rig.service.suspend("admin", &node("edge_7")).unwrap_err(),
        EnrolError::NodeRevoked
    );
    assert_eq!(
        rig.service.suspend("admin", &node("nobody")).unwrap_err(),
        EnrolError::NodeUnknown
    );
}

fn expiring_conditions(rig: &Rig) {
    let now = rig.clock.0.load(std::sync::atomic::Ordering::SeqCst);
    let cases = [
        ("exp_far", now + 365 * 86_400, None),
        ("exp_amber", now + 20 * 86_400, Some(Severity::Amber)),
        ("exp_red", now + 2 * 86_400, Some(Severity::Red)),
    ];
    for (name, not_after, _) in cases {
        let issued = rig
            .service
            .create_enrolment("admin", &request(name))
            .unwrap();
        let der = cert_between(name, now - 86_400, not_after);
        rig.service
            .redeem(&issued.code, &node(name), &der, "s")
            .unwrap();
    }
    let found = rig.service.expiring().unwrap();
    let got: Vec<(String, Severity)> = found
        .iter()
        .map(|e| (e.node.as_str().replace('-', "_"), e.severity))
        .collect();
    assert_eq!(
        got,
        vec![
            ("exp_red".to_owned(), Severity::Red),
            ("exp_amber".to_owned(), Severity::Amber)
        ]
    );
    assert_eq!(found[0].fingerprint.len(), 64);
}

fn the_directory_fails_closed(rig: &Rig) {
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_8"))
        .unwrap();
    let id = identity("edge_8");
    rig.service
        .redeem(&issued.code, &node("edge_8"), id.certificate_der(), "s")
        .unwrap();
    assert!(rig.directory.resolve(&client_cert(&id)).is_ok());
    // Not refreshed for a while: still served up to 30 s, then nobody.
    rig.clock.advance(20);
    assert!(rig.directory.needs_refresh());
    assert!(rig.directory.resolve(&client_cert(&id)).is_ok());
    rig.clock.advance(11);
    assert_eq!(
        rig.directory.resolve(&client_cert(&id)).unwrap_err().code,
        "sync_http.identity_unknown"
    );
    rig.directory.refresh().unwrap();
    assert!(rig.directory.resolve(&client_cert(&id)).is_ok());
}

pub fn _fingerprint(der: &[u8]) -> String {
    let _ = T0;
    fingerprint_of(der)
}
