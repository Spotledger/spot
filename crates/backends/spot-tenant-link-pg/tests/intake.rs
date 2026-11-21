//! The host intake on PostgreSQL (WP-CTL-10.T7): a handshake message is claimed through
//! `claim_inbound` before the handshake sees it, a remote revoke goes through `receive_revoke`,
//! the prune call removes only old nonces, and every refusal is one of the three coarse shapes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::{Chain, Env, env, linked, nonce, runtime, tenant};
use spot_link_transport::{IntakeRefusal, LinkIntake};
use spot_tenant_link::{
    ConsentPolicy, HandshakeMessage, IdentitySigner, LinkId, LinkState, LinkStore,
    MemoryPeerKeyDirectory, NonceClaim, Party, RevokeMessage, RevokeReason, SeedSigner,
    SignatureBytes, Trust,
};
use spot_tenant_link_pg::{HandshakeSink, PgLinkIntake, PgLinkStore};

#[derive(Default)]
struct Recorder {
    delivered: AtomicUsize,
    fail: AtomicBool,
    last: Mutex<Option<Party>>,
    claim: Mutex<Option<NonceClaim>>,
}

impl HandshakeSink for Recorder {
    fn deliver(
        &self,
        peer: &Party,
        _: &HandshakeMessage,
        claim: &NonceClaim,
        _: u64,
    ) -> Result<(), IntakeRefusal> {
        *self.claim.lock().unwrap() = Some(claim.clone());
        if self.fail.load(Ordering::SeqCst) {
            return Err(IntakeRefusal::Unavailable);
        }
        self.delivered.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(peer.clone());
        Ok(())
    }
}

fn nonces(e: &Env, store: &PgLinkStore, id: &LinkId) -> u64 {
    e.rt.block_on(store.nonce_count(id)).unwrap()
}

fn peer_signer() -> SeedSigner {
    SeedSigner::from_seed([2u8; 32], tenant(2))
}

fn intake_over(
    store: PgLinkStore,
    keys: MemoryPeerKeyDirectory,
    sink: &Arc<Recorder>,
) -> (PgLinkIntake, Arc<PgLinkStore>, Arc<Chain>) {
    let store = Arc::new(store);
    let chain = Arc::new(Chain::default());
    let intake = PgLinkIntake::new(
        Arc::clone(&store),
        chain.clone(),
        Arc::new(keys),
        Trust::PinnedOnly,
        sink.clone(),
    );
    (intake, store, chain)
}

#[test]
fn a_forged_proposal_adds_no_nonce_and_never_reaches_the_handshake() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let handshake = linked();
    let key_a = SeedSigner::from_seed([1u8; 32], tenant(1)).public_key();
    let keys = MemoryPeerKeyDirectory::with(tenant(1), key_a).unwrap();
    let sink = Arc::new(Recorder::default());
    let (intake, _store, _) = intake_over(e.store(2), keys, &sink);

    let mut forged = handshake.messages[0].clone();
    forged.signature = SignatureBytes::from_bytes([3u8; 64]);
    assert_eq!(
        intake.handshake(&tenant(1), &forged, 2_000),
        Err(IntakeRefusal::Forged)
    );
    // A proposal from a key nobody pinned, with the same tenant as sender.
    let unpinned = intake_over(
        e.store(2),
        MemoryPeerKeyDirectory::new(),
        &Arc::new(Recorder::default()),
    )
    .0;
    assert_eq!(
        unpinned.handshake(&tenant(1), &handshake.messages[0], 2_000),
        Err(IntakeRefusal::Forged)
    );
    assert_eq!(sink.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 0);
}

#[test]
fn a_genuine_proposal_is_claimed_once_and_a_replay_is_replayed() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let handshake = linked();
    let key_a = SeedSigner::from_seed([1u8; 32], tenant(1)).public_key();
    let keys = MemoryPeerKeyDirectory::with(tenant(1), key_a).unwrap();
    let sink = Arc::new(Recorder::default());
    let (intake, _store, _) = intake_over(e.store(2), keys, &sink);

    let propose = &handshake.messages[0];
    assert_eq!(intake.handshake(&tenant(1), propose, 2_000), Ok(()));
    assert_eq!(sink.delivered.load(Ordering::SeqCst), 1);
    assert_eq!(sink.last.lock().unwrap().clone(), Some(tenant(1)));
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 1);
    assert_eq!(
        intake.handshake(&tenant(1), propose, 2_001),
        Err(IntakeRefusal::Replayed)
    );
    assert_eq!(
        sink.delivered.load(Ordering::SeqCst),
        1,
        "a replay is not handed on"
    );
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 1);
}

#[test]
fn a_message_the_handshake_could_not_take_gives_its_nonce_back() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let handshake = linked();
    let key_a = SeedSigner::from_seed([1u8; 32], tenant(1)).public_key();
    let keys = MemoryPeerKeyDirectory::with(tenant(1), key_a).unwrap();
    let sink = Arc::new(Recorder::default());
    let (intake, _store, _) = intake_over(e.store(2), keys, &sink);
    let propose = &handshake.messages[0];

    sink.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        intake.handshake(&tenant(1), propose, 2_000),
        Err(IntakeRefusal::Unavailable)
    );
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 0);
    // The claim the sink saw is dead: it cannot be spent after the nonce was given back.
    let dead = sink.claim.lock().unwrap().take().unwrap();
    let (log, _) = e.store(2).handshake_ports();
    assert!(
        !spot_tenant_link::NonceLog::record_claimed(&log, dead.link(), &propose.nonce, &dead)
            .unwrap()
    );
    sink.fail.store(false, Ordering::SeqCst);
    assert_eq!(intake.handshake(&tenant(1), propose, 2_001), Ok(()));
}

#[test]
fn a_revoke_signed_by_a_stranger_leaves_the_link_active_and_a_genuine_one_ends_it_once() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let id = store.insert(linked().a).unwrap().id;
    let sink = Arc::new(Recorder::default());
    let (intake, store, _chain) = intake_over(store, MemoryPeerKeyDirectory::new(), &sink);

    let stranger = SeedSigner::from_seed([9u8; 32], tenant(2));
    let forged = RevokeMessage::sign(&stranger, id, RevokeReason::TermsEnded, nonce(40));
    assert_eq!(
        intake.revoke(&tenant(2), &forged, 5_000),
        Err(IntakeRefusal::Forged)
    );
    assert_eq!(store.get(&id).unwrap().state, LinkState::Active);
    assert_eq!(nonces(&e, &store, &id), 0, "a stranger writes no nonce");

    let genuine = RevokeMessage::sign(&peer_signer(), id, RevokeReason::SecurityConcern, nonce(43));
    assert_eq!(intake.revoke(&tenant(2), &genuine, 5_001), Ok(()));
    assert_eq!(store.get(&id).unwrap().state, LinkState::Revoked);
    assert_eq!(nonces(&e, &store, &id), 1);
    assert_eq!(
        intake.revoke(&tenant(2), &genuine, 5_002),
        Err(IntakeRefusal::Stale),
        "a replay changes nothing"
    );
    assert_eq!(store.get(&id).unwrap().version, 2);
}

#[test]
fn the_prune_call_removes_only_nonces_older_than_the_window() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let handshake = linked();
    let sink = Arc::new(Recorder::default());
    let store = e.store(1);
    let id = store.insert(handshake.a.clone()).unwrap().id;
    let (intake, store, _) = intake_over(store, MemoryPeerKeyDirectory::new(), &sink);
    let policy = ConsentPolicy::default();
    let window = policy.max_age_ms();
    let now = 5 * window;
    let schema = common::tenant_id(1).schema_name();
    for (byte, seen_at) in [("b1", now - window - 5), ("b2", now - window), ("b3", now)] {
        e.rt.block_on(async {
            let client = e.db.raw_client().await;
            client
                .batch_execute(&format!(
                    "SET search_path TO \"{schema}\"; \
                     INSERT INTO spot_tenant_link_nonce (link, nonce, seen_at) \
                     VALUES ('{id}', decode(repeat('{byte}', 16), 'hex'), {seen_at})"
                ))
                .await
                .unwrap();
        });
    }
    assert_eq!(intake.prune_nonces(now, policy), Ok(1));
    assert_eq!(nonces(&e, &store, &id), 2);
    assert_eq!(intake.prune_nonces(now, policy), Ok(0));
}
