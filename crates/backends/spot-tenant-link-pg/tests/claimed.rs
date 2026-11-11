//! The claimed-aware nonce path (WP-CTL-10.T8): the nonce `claim_inbound` claimed is recorded by
//! the handshake through the caller's claim, once, for its own nonce and link, never after the
//! claim was released, and a replay of the same bytes is still refused.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Chain, Env, env, link_id, linked, nonce, runtime, tenant};
use spot_iam_access::StepUpPolicy;
use spot_link_transport::{IntakeRefusal, LinkIntake};
use spot_tenant_link::{
    ConsentPolicy, Error, Handshake, HandshakeMessage, HandshakePolicy, IdentitySigner,
    MemoryAuditLog, MemoryNonceLog, MemoryPeerKeyDirectory, NonceClaim, NonceLog, Party,
    PolicyStepUp, SeedSigner, Trust,
};
use spot_tenant_link_pg::{HandshakeSink, PgLinkIntake, PgLinkStore};

/// The host's driver for the invited side: `from_proposal_claimed` over the store's own log.
struct ProposalSink {
    store: Arc<PgLinkStore>,
    applied: AtomicUsize,
}

impl HandshakeSink for ProposalSink {
    fn deliver(
        &self,
        _: &Party,
        message: &HandshakeMessage,
        claim: &NonceClaim,
        at: u64,
    ) -> Result<(), IntakeRefusal> {
        let (log, keys) = self.store.handshake_ports();
        let key_a = SeedSigner::from_seed([1u8; 32], tenant(1)).public_key();
        let signer = SeedSigner::from_seed([2u8; 32], tenant(2));
        let audit = MemoryAuditLog::new();
        let gate = PolicyStepUp::new(StepUpPolicy::default());
        let policy = HandshakePolicy {
            keys: &keys,
            trust: Trust::first_use(key_a),
            consent: ConsentPolicy::default(),
            gate: &gate,
            nonces: &log,
        };
        Handshake::from_proposal_claimed(&signer, &audit, &policy, message, at, Some(claim))
            .map(|_| {
                self.applied.fetch_add(1, Ordering::SeqCst);
            })
            .map_err(|error| match error {
                Error::Replayed { .. } => IntakeRefusal::Replayed,
                _ => IntakeRefusal::Forged,
            })
    }
}

fn key_directory() -> MemoryPeerKeyDirectory {
    let key_a = SeedSigner::from_seed([1u8; 32], tenant(1)).public_key();
    MemoryPeerKeyDirectory::with(tenant(1), key_a).unwrap()
}

fn claim_of(e: &Env, store: &PgLinkStore, message: &HandshakeMessage) -> NonceClaim {
    e.rt.block_on(store.claim_inbound(message, &key_directory(), &Trust::PinnedOnly, 2_000))
        .unwrap()
}

#[test]
fn a_full_intake_over_the_pg_log_applies_the_message_once_and_a_resend_is_replayed() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let propose = linked().messages[0].clone();
    let store = Arc::new(e.store(2));
    let sink = Arc::new(ProposalSink {
        store: Arc::clone(&store),
        applied: AtomicUsize::new(0),
    });
    let intake = PgLinkIntake::new(
        Arc::clone(&store),
        Arc::new(Chain::default()),
        Arc::new(key_directory()),
        Trust::PinnedOnly,
        sink.clone(),
    );
    assert_eq!(intake.handshake(&tenant(1), &propose, 2_000), Ok(()));
    assert_eq!(sink.applied.load(Ordering::SeqCst), 1);
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 1);
    assert_eq!(
        intake.handshake(&tenant(1), &propose, 2_001),
        Err(IntakeRefusal::Replayed)
    );
    assert_eq!(sink.applied.load(Ordering::SeqCst), 1);
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 1);
}

#[test]
fn without_the_claim_the_handshake_still_reads_the_claimed_nonce_as_a_replay() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let propose = linked().messages[0].clone();
    let store = e.store(2);
    let _claim = claim_of(&e, &store, &propose);
    let (log, keys) = store.handshake_ports();
    let key_a = SeedSigner::from_seed([1u8; 32], tenant(1)).public_key();
    let gate = PolicyStepUp::new(StepUpPolicy::default());
    let policy = HandshakePolicy {
        keys: &keys,
        trust: Trust::first_use(key_a),
        consent: ConsentPolicy::default(),
        gate: &gate,
        nonces: &log,
    };
    let signer = SeedSigner::from_seed([2u8; 32], tenant(2));
    let audit = MemoryAuditLog::new();
    let result = Handshake::from_proposal_claimed(&signer, &audit, &policy, &propose, 2_000, None);
    assert!(matches!(result, Err(Error::Replayed { .. })));
}

#[test]
fn a_claim_is_spent_once_and_only_for_its_own_nonce_and_link() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let propose = linked().messages[0].clone();
    let link = link_id(1);
    let store = e.store(2);
    let claim = claim_of(&e, &store, &propose);
    let (log, _) = store.handshake_ports();

    // Another nonce, another link: refused, and the claim stays unspent.
    assert!(!log.record_claimed(&link, &nonce(99), &claim).unwrap());
    assert!(
        !log.record_claimed(&link_id(9), &propose.nonce, &claim)
            .unwrap()
    );
    // Its own nonce and link: accepted once.
    assert!(log.record_claimed(&link, &propose.nonce, &claim).unwrap());
    assert!(!log.record_claimed(&link, &propose.nonce, &claim).unwrap());
    // A copy shares the state.
    assert!(
        !log.record_claimed(&link, &propose.nonce, &claim.clone())
            .unwrap()
    );
    // The plain record still refuses the nonce: nothing became idempotent.
    assert!(!log.record(&link, &propose.nonce).unwrap());
}

#[test]
fn a_released_claim_is_dead() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let propose = linked().messages[0].clone();
    let store = e.store(2);
    let claim = claim_of(&e, &store, &propose);
    let (log, _) = store.handshake_ports();
    claim.release();
    assert!(
        !log.record_claimed(&link_id(1), &propose.nonce, &claim)
            .unwrap()
    );
}

#[test]
fn a_claim_for_a_nonce_the_tenant_never_claimed_is_refused() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let propose = linked().messages[0].clone();
    let claim = claim_of(&e, &e.store(2), &propose);
    // Tenant 1's log holds no such row, so the same claim is refused there.
    let (other_log, _) = e.store(1).handshake_ports();
    assert!(
        !other_log
            .record_claimed(&link_id(1), &propose.nonce, &claim)
            .unwrap()
    );
    // A claim won in an in-memory log for a nonce the pg log never held is refused too, and so
    // is a claim for another nonce or another link taken in the tenant's own memory log.
    let memory = MemoryNonceLog::new();
    let forged = memory.claim(&link_id(1), &nonce(77)).unwrap().unwrap();
    assert!(
        !other_log
            .record_claimed(&link_id(1), &nonce(77), &forged)
            .unwrap()
    );
    let wrong_link = memory.claim(&link_id(2), &nonce(78)).unwrap().unwrap();
    assert!(
        !other_log
            .record_claimed(&link_id(1), &nonce(78), &wrong_link)
            .unwrap()
    );
}
