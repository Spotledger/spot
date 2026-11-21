//! The register items recorded security findings on PostgreSQL: a revocation whose party and actor are
//! derived, a nonce claimed only after the message proves itself (and pruned only outside the
//! replay window), a denied list that leaves a trace, and an acceptance that is one transaction.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Mutex;

use common::{Chain, Env, consent, env, link_id, linked, nonce, offer, runtime, tenant};
use spot_audit_chain::AuditRecord;
use spot_iam_access::StepUpPolicy;
use spot_permissions::Principal;
use spot_tenant_link::{
    AuditSink, ConsentPolicy, Direction, Error, HandshakeBody, HandshakeMessage, IdentitySigner,
    LinkId, LinkState, LinkStore, MemoryPeerKeyDirectory, PolicyStepUp, RevokeMessage,
    RevokeReason, SeedSigner, SignatureBytes, Trust,
};
use spot_tenant_link_pg::{
    Caller, LinkAdmin, LinkPermissions, OpError, Operation, PERMISSION_DENIED, PgLinkStore,
};

struct Allow;
impl LinkPermissions for Allow {
    fn may(&self, _: &Principal, _: Operation, _: Option<&LinkId>) -> bool {
        true
    }
}

struct Deny;
impl LinkPermissions for Deny {
    fn may(&self, _: &Principal, _: Operation, _: Option<&LinkId>) -> bool {
        false
    }
}

fn gate() -> PolicyStepUp {
    PolicyStepUp::new(StepUpPolicy::default())
}

fn caller(name: &str) -> Caller {
    Caller::new(Principal::with_roles(name, &["Link Admin"]))
}

fn peer_signer() -> SeedSigner {
    SeedSigner::from_seed([2u8; 32], tenant(2))
}

/// Tenant 1's store holding its finished (active) copy of the link.
fn active_at_a(e: &Env) -> (PgLinkStore, LinkId) {
    let store = e.store(1);
    let link = store.insert(linked().a).unwrap();
    (store, link.id)
}

fn nonces(e: &Env, store: &PgLinkStore, id: &LinkId) -> u64 {
    e.rt.block_on(store.nonce_count(id)).unwrap()
}

// Recorded security finding ---------------------------------------------------------------------------------------

#[test]
fn a_local_revoke_takes_the_party_and_the_actor_from_the_store_and_the_caller() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &Allow);
    let revoked = admin
        .revoke(&caller("root"), &id, RevokeReason::TermsEnded, 5_000)
        .unwrap();
    let revocation = revoked.revocation.unwrap();
    assert_eq!(revocation.by, store.local_party());
    assert_eq!(revoked.state, LinkState::Revoked);
    let records = chain.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].actor, "user:root", "the authenticated caller");
}

#[test]
fn a_genuine_remote_revoke_is_effective_at_once_and_a_forged_or_replayed_one_changes_nothing() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &Deny);

    // Forged: a stranger's key, a broken signature, a stale revision.
    let stranger = SeedSigner::from_seed([9u8; 32], tenant(2));
    let forged = [
        RevokeMessage::sign(&stranger, id, RevokeReason::TermsEnded, nonce(40)),
        {
            let mut m =
                RevokeMessage::sign(&peer_signer(), id, RevokeReason::TermsEnded, nonce(41));
            m.signature = SignatureBytes::from_bytes([0u8; 64]);
            m
        },
        {
            let mut m =
                RevokeMessage::sign(&peer_signer(), id, RevokeReason::TermsEnded, nonce(42));
            m.revision = 2;
            m.signature = peer_signer().sign(&m.signing_bytes());
            m
        },
    ];
    for message in &forged {
        assert!(admin.receive_revoke(message, 5_000).is_err());
    }
    assert_eq!(store.get(&id).unwrap().state, LinkState::Active);
    assert_eq!(store.get(&id).unwrap().version, 1);
    assert_eq!(
        nonces(&e, &store, &id),
        0,
        "no forged message spent a nonce"
    );

    // Genuine: effective on return, attributed to the peer, its nonce spent.
    let genuine = RevokeMessage::sign(&peer_signer(), id, RevokeReason::SecurityConcern, nonce(43));
    let revoked = admin.receive_revoke(&genuine, 5_001).unwrap();
    assert_eq!(revoked.state, LinkState::Revoked);
    assert_eq!(revoked.revocation.unwrap().by, tenant(2));
    assert_eq!(store.get(&id).unwrap().state, LinkState::Revoked);
    assert_eq!(nonces(&e, &store, &id), 1);

    // Replayed: the nonce is spent and the link is already ended; nothing changes.
    let replay = admin.receive_revoke(&genuine, 5_002).unwrap_err();
    assert!(
        matches!(replay, OpError::Link(Error::NotActive { .. })),
        "{replay}"
    );
    assert_eq!(store.get(&id).unwrap().version, 2);
}

#[test]
fn a_replayed_nonce_is_refused_before_the_link_changes() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &Deny);
    let message = RevokeMessage::sign(&peer_signer(), id, RevokeReason::TermsEnded, nonce(44));
    // The nonce is already recorded for this link (a first delivery whose commit never landed).
    e.rt.block_on(async {
        let client = e.db.raw_client().await;
        client
            .batch_execute(&format!(
                "SET search_path TO \"{}\"; \
                 INSERT INTO spot_tenant_link_nonce (link, nonce, seen_at) \
                 VALUES ('{}', decode(repeat('2c', 16), 'hex'), 5000)",
                common::tenant_id(1).schema_name(),
                id
            ))
            .await
            .unwrap();
    });
    let error = admin.receive_revoke(&message, 5_001).unwrap_err();
    assert!(
        matches!(error, OpError::Link(Error::Replayed { .. })),
        "{error}"
    );
    assert_eq!(store.get(&id).unwrap().state, LinkState::Active);
}

// Recorded security finding ---------------------------------------------------------------------------------------

#[test]
fn a_message_that_does_not_verify_leaves_the_nonce_table_unchanged() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let handshake = linked();
    let accept = &handshake.messages[1];
    let keys = MemoryPeerKeyDirectory::new();

    // A byte of the signature changed.
    let mut tampered = accept.clone();
    let mut bytes = *tampered.signature.as_bytes();
    bytes[0] ^= 1;
    tampered.signature = SignatureBytes::from_bytes(bytes);
    let error =
        e.rt.block_on(store.claim_inbound(&tampered, &keys, &Trust::PinnedOnly, 2_000))
            .unwrap_err();
    assert!(matches!(error, Error::BadSignature), "{error:?}");

    // A well-signed message of a stranger's key for this link.
    let stranger = SeedSigner::from_seed([9u8; 32], tenant(2));
    let forged = HandshakeMessage {
        signer: stranger.public_key(),
        signature: SignatureBytes::from_bytes([0u8; 64]),
        ..accept.clone()
    };
    let mut forged = forged;
    forged.signature = stranger.sign(&forged.signing_bytes());
    let error =
        e.rt.block_on(store.claim_inbound(&forged, &keys, &Trust::PinnedOnly, 2_000))
            .unwrap_err();
    assert!(matches!(error, Error::WrongSigner { .. }), "{error:?}");

    // A well-signed message for a link this tenant does not hold.
    let mut elsewhere = accept.clone();
    if let HandshakeBody::Accept(body) = &mut elsewhere.body {
        body.link = link_id(99);
    }
    let signer = peer_signer();
    elsewhere.signature = signer.sign(&elsewhere.signing_bytes());
    let error =
        e.rt.block_on(store.claim_inbound(&elsewhere, &keys, &Trust::PinnedOnly, 2_000))
            .unwrap_err();
    assert!(matches!(error, Error::NotFound { .. }), "{error:?}");

    // A proposal addressed to somebody else, and one from a key nobody pinned.
    let propose = &handshake.messages[0];
    let error =
        e.rt.block_on(store.claim_inbound(propose, &keys, &Trust::PinnedOnly, 2_000))
            .unwrap_err();
    assert!(
        matches!(error, Error::InvalidParty { .. }),
        "the proposal is for tenant 2, not for tenant 1: {error:?}"
    );
    let store_b = e.store(2);
    let error =
        e.rt.block_on(store_b.claim_inbound(propose, &keys, &Trust::PinnedOnly, 2_000))
            .unwrap_err();
    assert!(matches!(error, Error::KeyUnpinned { .. }), "{error:?}");

    assert_eq!(nonces(&e, &store, &id), 0);
    assert_eq!(e.raw_count(2, "spot_tenant_link_nonce"), 0);

    // The genuine messages claim, once.
    e.rt.block_on(store.claim_inbound(accept, &keys, &Trust::PinnedOnly, 2_000))
        .unwrap();
    let pinned =
        MemoryPeerKeyDirectory::with(tenant(1), handshake.a.peer_keys[0].unwrap()).unwrap();
    e.rt.block_on(store_b.claim_inbound(propose, &pinned, &Trust::PinnedOnly, 2_000))
        .unwrap();
    let replay =
        e.rt.block_on(store.claim_inbound(accept, &keys, &Trust::PinnedOnly, 2_001))
            .unwrap_err();
    assert!(matches!(replay, Error::Replayed { .. }));
    assert_eq!(nonces(&e, &store, &id), 1);
}

#[test]
fn pruning_removes_only_the_nonces_older_than_the_replay_window() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let schema = common::tenant_id(1).schema_name();
    let policy = ConsentPolicy::default();
    let window = policy.max_age_ms();
    let now = 5 * window;
    let cutoff = now - window;
    // A genuine message claimed inside the window.
    let accept = linked().messages[1].clone();
    let keys = MemoryPeerKeyDirectory::new();
    e.rt.block_on(store.claim_inbound(&accept, &keys, &Trust::PinnedOnly, now))
        .unwrap();
    // Two outside the window, one exactly at its edge, two inside.
    for (byte, seen_at) in [
        ("a1", cutoff - window),
        ("a2", cutoff - 1),
        ("a3", cutoff),
        ("a4", cutoff + 1),
        ("a5", now),
    ] {
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
    assert_eq!(nonces(&e, &store, &id), 6);
    let removed = e.rt.block_on(store.prune_nonces(now, policy)).unwrap();
    assert_eq!(removed, 2, "only the two older than the window");
    assert_eq!(nonces(&e, &store, &id), 4);
    // Nothing inside the window goes, however often it runs.
    assert_eq!(e.rt.block_on(store.prune_nonces(now, policy)).unwrap(), 0);
    // A clock that is behind the window prunes nothing.
    assert_eq!(e.rt.block_on(store.prune_nonces(10, policy)).unwrap(), 0);
    assert_eq!(nonces(&e, &store, &id), 4);
    // A nonce inside the window is still a replay after pruning.
    let replay =
        e.rt.block_on(store.claim_inbound(&accept, &keys, &Trust::PinnedOnly, now + 1))
            .unwrap_err();
    assert!(matches!(replay, Error::Replayed { .. }), "{replay:?}");
}

// Recorded security finding ---------------------------------------------------------------------------------------

#[test]
fn a_denied_list_leaves_one_audit_reference_and_a_failed_write_still_returns_the_denial() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &Deny);
    let clerk = caller("clerk");

    let error = admin.list(&clerk, 7_000).unwrap_err();
    assert_eq!(error.code(), PERMISSION_DENIED);
    let records = chain.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, PERMISSION_DENIED);
    assert_eq!(records[0].actor, "user:clerk");
    assert_eq!(records[0].at, 7_000);
    assert!(records[0].subject.is_none(), "it names no link");
    let refs = e.rt.block_on(store.denied_audit_refs()).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].kind, PERMISSION_DENIED);
    assert_eq!(refs[0].actor, "user:clerk");
    assert_eq!(refs[0].at, 7_000);

    // The chain is down: the caller still gets the denial, not the audit failure.
    chain.set_failing(true);
    let error = admin.list(&clerk, 7_001).unwrap_err();
    assert!(matches!(error, OpError::Denied { .. }), "{error}");
    assert_eq!(e.rt.block_on(store.denied_audit_refs()).unwrap().len(), 1);
    chain.set_failing(false);

    // A denied show and a denied audit read leave their reference under the link asked about.
    let id = link_id(1);
    assert!(admin.show(&clerk, &id, 7_002).is_err());
    assert!(admin.show_audit(&clerk, &id, 7_003).is_err());
    let refused: Vec<_> =
        e.rt.block_on(store.audit_refs(&id))
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
    assert_eq!(refused, vec!["tenant_link.refused", "tenant_link.refused"]);
    // The unnamed list still holds exactly its one.
    assert_eq!(e.rt.block_on(store.denied_audit_refs()).unwrap().len(), 1);
}

// Recorded security finding ---------------------------------------------------------------------------------------

/// A chain that, on its first append, moves the link on (as a second administrator would), so the
/// version the acceptance read is stale when it commits.
struct Racing<'a> {
    chain: Chain,
    store: &'a PgLinkStore,
    fired: Mutex<bool>,
    id: LinkId,
}

impl AuditSink for Racing<'_> {
    fn append(&self, record: AuditRecord) -> spot_tenant_link::Result<u64> {
        let mut fired = self.fired.lock().unwrap();
        if !*fired {
            *fired = true;
            let link = self.store.get(&self.id).unwrap();
            let next = link
                .revoked_by(&link.local, RevokeReason::TermsEnded, 1_900)
                .unwrap();
            self.store.update(next, link.version).unwrap();
        }
        self.chain.append(record)
    }
}

/// Tenant `n`'s copy of a proposal from tenant 1, with the proposer's offer on record.
fn proposal_at(e: &Env, n: u8) -> (PgLinkStore, LinkId) {
    let store = e.store(n);
    let mut link = linked().b;
    link.local = tenant(n);
    link.parties = {
        let mut parties = [tenant(1), tenant(n)];
        parties.sort();
        parties
    };
    link.state = LinkState::Proposed;
    link.grants.clear();
    link.consents.truncate(1);
    link.peer_keys = [None, None];
    link.version = 0;
    let stored = store.insert(link).unwrap();
    e.rt.block_on(store.put_offer(&stored.id, &offer(&tenant(1), Direction::Send)))
        .unwrap();
    (store, stored.id)
}

fn local_offer_on_record(e: &Env, store: &PgLinkStore, id: &LinkId) -> bool {
    e.rt.block_on(store.offers(id)).unwrap().0.is_some()
}

#[test]
fn accept_is_one_transaction_a_conflict_or_a_failed_audit_leaves_no_offer_row() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let g = gate();
    let b = caller("b");

    // A version conflict between the read and the commit.
    let (store, id) = proposal_at(&e, 2);
    let racing = Racing {
        chain: Chain::default(),
        store: &store,
        fired: Mutex::new(false),
        id,
    };
    let admin = LinkAdmin::new(&store, &racing, &g, &Allow);
    let error = admin
        .accept(
            &b,
            &id,
            offer(&tenant(2), Direction::Receive),
            consent(&tenant(2), "user:b", 1_300),
            1_400,
        )
        .unwrap_err();
    assert!(
        matches!(error, OpError::Link(Error::VersionConflict { .. })),
        "{error}"
    );
    assert!(
        !local_offer_on_record(&e, &store, &id),
        "no offer row without the transition"
    );
    assert_eq!(
        store.get(&id).unwrap().state,
        LinkState::Revoked,
        "the racer's change stands"
    );

    // A failing audit write: nothing is stored at all.
    let (store, id) = proposal_at(&e, 3);
    let chain = Chain::default();
    chain.set_failing(true);
    let admin = LinkAdmin::new(&store, &chain, &g, &Allow);
    let error = admin
        .accept(
            &b,
            &id,
            offer(&tenant(3), Direction::Receive),
            consent(&tenant(3), "user:b", 1_300),
            1_400,
        )
        .unwrap_err();
    assert!(
        matches!(error, OpError::Link(Error::AuditFailed { .. })),
        "{error}"
    );
    assert!(!local_offer_on_record(&e, &store, &id));
    assert_eq!(store.get(&id).unwrap().state, LinkState::Proposed);

    // And the success path keeps the offer, the grants, the consent and the state together.
    chain.set_failing(false);
    let accepted = admin
        .accept(
            &b,
            &id,
            offer(&tenant(3), Direction::Receive),
            consent(&tenant(3), "user:b", 1_300),
            1_400,
        )
        .unwrap();
    assert_eq!(accepted.state, LinkState::AcceptedByPeer);
    assert_eq!(accepted.consents.len(), 2);
    assert!(!accepted.grants.is_empty());
    assert!(local_offer_on_record(&e, &store, &id));
}

#[test]
fn a_remote_revoke_whose_audit_write_fails_leaves_no_burned_nonce_and_a_resend_applies() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &Deny);
    let message = RevokeMessage::sign(&peer_signer(), id, RevokeReason::SecurityConcern, nonce(90));
    chain.set_failing(true);
    assert!(admin.receive_revoke(&message, 5_000).is_err());
    assert_eq!(nonces(&e, &store, &id), 0, "the nonce was released");
    assert_eq!(store.get(&id).unwrap().state, LinkState::Active);
    chain.set_failing(false);
    let revoked = admin.receive_revoke(&message, 5_001).unwrap();
    assert_eq!(revoked.state, LinkState::Revoked);
    assert_eq!(nonces(&e, &store, &id), 1);
}

#[test]
fn a_stranger_signed_revoke_leaves_no_audit_row_and_no_nonce() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (store, id) = active_at_a(&e);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &Deny);
    let stranger = SeedSigner::from_seed([9u8; 32], tenant(2));
    let message = RevokeMessage::sign(&stranger, id, RevokeReason::TermsEnded, nonce(91));
    assert!(admin.receive_revoke(&message, 5_000).is_err());
    assert!(chain.records().is_empty());
    assert_eq!(nonces(&e, &store, &id), 0);
    assert!(e.rt.block_on(store.audit_refs(&id)).unwrap().is_empty());
}
