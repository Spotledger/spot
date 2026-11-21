//! The claimed-aware nonce path (WP-CTL-10.T8) over the in-memory log: a caller that claimed a
//! message's nonce itself passes the claim to the handshake, which records it exactly once.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Ports, consent, link_id, nonce, offer, tenant};
use spot_tenant_link::{
    Carrier, Direction, Error, Handshake, HandshakeMessage, IdentitySigner, LinkDraft,
    MemoryAuditLog, MemoryNonceLog, NonceClaim, NonceLog, SeedSigner,
};

struct Rig {
    key_b: SeedSigner,
    ports_b: Ports,
    propose: HandshakeMessage,
}

fn rig() -> Rig {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let audit = MemoryAuditLog::new();
    let draft = LinkDraft::new(
        link_id(1),
        a.clone(),
        b,
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (_, propose) = Handshake::propose(
        &key_a,
        &audit,
        &ports_a.policy(),
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    Rig {
        key_b,
        ports_b,
        propose,
    }
}

fn take(r: &Rig, claim: Option<&NonceClaim>) -> Result<(), Error> {
    let audit = MemoryAuditLog::new();
    Handshake::from_proposal_claimed(
        &r.key_b,
        &audit,
        &r.ports_b.policy(),
        &r.propose,
        1_200,
        claim,
    )
    .map(|_| ())
}

#[test]
fn a_claimed_proposal_is_taken_once_through_its_claim() {
    let r = rig();
    let claim = r
        .ports_b
        .log
        .claim(&link_id(1), &r.propose.nonce)
        .unwrap()
        .unwrap();
    // Without the claim the nonce reads as a replay: record is not idempotent.
    assert!(matches!(take(&r, None), Err(Error::Replayed { .. })));
    take(&r, Some(&claim)).unwrap();
    // The same claim, and a fresh message with the same nonce, are replays now.
    assert!(matches!(
        take(&r, Some(&claim)),
        Err(Error::Replayed { .. })
    ));
    assert!(matches!(take(&r, None), Err(Error::Replayed { .. })));
}

#[test]
fn a_claim_for_another_nonce_or_link_or_after_release_is_refused() {
    let r = rig();
    let log = &r.ports_b.log;
    let claim = log.claim(&link_id(1), &r.propose.nonce).unwrap().unwrap();
    // The claim of a different nonce held in the log does not carry this message.
    let other = log.claim(&link_id(1), &nonce(50)).unwrap().unwrap();
    assert!(matches!(
        take(&r, Some(&other)),
        Err(Error::Replayed { .. })
    ));
    // A claim for another link, taken for this very nonce in the same log.
    let elsewhere = log.claim(&link_id(2), &r.propose.nonce).unwrap().unwrap();
    assert!(matches!(
        take(&r, Some(&elsewhere)),
        Err(Error::Replayed { .. })
    ));
    // A released claim.
    claim.release();
    assert!(matches!(
        take(&r, Some(&claim)),
        Err(Error::Replayed { .. })
    ));
}

#[test]
fn a_claim_for_a_nonce_the_log_never_held_is_refused() {
    let r = rig();
    // A claim won in another log: right link, right nonce, but this log never held the nonce.
    let forged = MemoryNonceLog::new()
        .claim(&link_id(1), &r.propose.nonce)
        .unwrap()
        .unwrap();
    assert!(matches!(
        take(&r, Some(&forged)),
        Err(Error::Replayed { .. })
    ));
    assert!(
        !r.ports_b
            .log
            .contains(&link_id(1), &r.propose.nonce)
            .unwrap()
    );
    // The plain path is untouched: the message is still taken normally.
    take(&r, None).unwrap();
}

#[test]
fn a_log_without_claim_support_refuses_a_claim() {
    struct Plain;
    impl NonceLog for Plain {
        fn record(
            &self,
            _: &spot_tenant_link::LinkId,
            _: &spot_tenant_link::Nonce,
        ) -> Result<bool, Error> {
            Ok(true)
        }
        fn release(
            &self,
            _: &spot_tenant_link::LinkId,
            _: &spot_tenant_link::Nonce,
        ) -> Result<(), Error> {
            Ok(())
        }
    }
    let claim = MemoryNonceLog::new()
        .claim(&link_id(1), &nonce(1))
        .unwrap()
        .unwrap();
    assert!(matches!(
        Plain.record_claimed(&link_id(1), &nonce(1), &claim),
        Err(Error::StoreFailed { .. })
    ));
}

#[test]
fn a_claim_is_only_obtainable_from_a_store_primitive_that_inserted_the_nonce() {
    use spot_tenant_link::{NonceClaimer, claim_with};
    // A store that reports "already there" yields no claim, whatever the caller does.
    struct Full;
    impl NonceClaimer for Full {
        fn insert_new(
            &self,
            _: &spot_tenant_link::LinkId,
            _: &spot_tenant_link::Nonce,
        ) -> Result<bool, Error> {
            Ok(false)
        }
    }
    assert!(claim_with(&Full, &link_id(1), &nonce(1)).unwrap().is_none());
    // A claim is for exactly the link and nonce it was taken for, and spends once.
    let log = MemoryNonceLog::new();
    let claim = claim_with(&log, &link_id(1), &nonce(1)).unwrap().unwrap();
    assert!(claim.is_for(&link_id(1), &nonce(1)));
    assert!(!claim.is_for(&link_id(2), &nonce(1)));
    assert!(!claim.is_for(&link_id(1), &nonce(2)));
    assert!(claim_with(&log, &link_id(1), &nonce(1)).unwrap().is_none());
}
