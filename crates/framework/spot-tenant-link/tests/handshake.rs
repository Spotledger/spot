//! The handshake end to end: activation, both-sides consent, mirrored grants, an external peer,
//! rejection, replay, tampering and a message meant for another link.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Ports, consent, link_id, linked, nonce, offer, tenant, topic};
use spot_tenant_link::{
    Carrier, Consent, Direction, Error, FieldList, FieldName, Grant, Handshake, IdentitySigner,
    Link, LinkDraft, LinkEventKind, LinkState, LinkStore, MemoryAuditLog, MemoryLinkStore, Offer,
    Party, SchemaRange, SeedSigner, TopicKind,
};

#[test]
fn a_full_handshake_activates_both_sides_with_mirrored_grants() {
    let linked = linked();
    let (a, b) = (linked.a, linked.b);
    assert_eq!(a.state, LinkState::Active);
    assert_eq!(b.state, LinkState::Active);
    assert_eq!(a.id, b.id);
    assert_eq!(a.parties, b.parties);
    assert_eq!(a.carrier, Carrier::Direct);
    assert_eq!(a.consents.len(), 2);
    assert_eq!(a.consents[0].actor, "user:a");
    assert_eq!(a.consents[1].actor, "user:b");
    assert_eq!(b.consents.len(), 2);
    assert_eq!(a.grants.len(), 1);
    assert_eq!(b.grants.len(), 1);
    assert_eq!(a.grants[0].direction, Direction::Send);
    assert_eq!(b.grants[0].direction, Direction::Receive);
    assert_eq!(a.grants[0].fields, b.grants[0].fields);
    assert_eq!(a.grants[0].expires, b.grants[0].expires);
    // Both identity keys, and they are different keys.
    assert!(a.peer_keys.iter().all(Option::is_some));
    assert!(b.peer_keys.iter().all(Option::is_some));
    assert_ne!(a.peer_keys[0], a.peer_keys[1]);
    // The single check agrees on both sides.
    assert!(a.allows(&topic(), Direction::Send, 2_000).is_ok());
    assert!(b.allows(&topic(), Direction::Receive, 2_000).is_ok());
    assert!(matches!(
        a.allows(&topic(), Direction::Receive, 2_000),
        Err(Error::WrongDirection { .. })
    ));
    assert!(matches!(
        a.allows(&topic(), Direction::Send, 100_000),
        Err(Error::LinkExpired { .. })
    ));
    // Both copies store.
    let store = MemoryLinkStore::new();
    let stored = store.insert(a.clone()).unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(
        store
            .find(&a.parties[0], &a.parties[1])
            .unwrap()
            .unwrap()
            .id,
        a.id
    );
}

#[test]
fn both_sides_reconstruct_the_same_history_from_their_own_audit() {
    let linked = linked();
    let first: Vec<LinkEventKind> = linked
        .audit_a
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    let second: Vec<LinkEventKind> = linked
        .audit_b
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(first, second);
    assert_eq!(
        first,
        vec![
            LinkEventKind::Proposed,
            LinkEventKind::Accepted,
            LinkEventKind::KeyExchanged,
            LinkEventKind::KeyExchanged,
            LinkEventKind::Activated,
        ]
    );
    // Every event names the same link on both sides.
    let ids: Vec<_> = linked
        .audit_a
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.link)
        .collect();
    assert!(ids.iter().all(|id| *id == linked.a.id));
}

#[test]
fn the_link_record_round_trips_through_json() {
    let link = linked().a;
    let text = serde_json::to_string(&link).unwrap();
    let back: Link = serde_json::from_str(&text).unwrap();
    assert_eq!(back, link);
    back.validate().unwrap();
}

#[test]
fn an_external_link_carries_message_topics_only() {
    let a = tenant(1);
    let peer = Party::External(spot_tenant_link::ExternalPeerId::new("connector-one").unwrap());
    let record = Grant {
        topic: topic(),
        kind: TopicKind::Record,
        direction: Direction::Send,
        schema_range: SchemaRange::one(1).unwrap(),
        fields: FieldList::only([FieldName::new("record.name").unwrap()]),
        expires: 9_000,
        require_peer_region: None,
        notify: false,
    };
    let refused = LinkDraft::new(
        link_id(2),
        a.clone(),
        peer.clone(),
        Carrier::External,
        1_000,
        None,
        Offer::new(a.clone(), vec![record]).unwrap(),
    );
    assert!(matches!(refused, Err(Error::ExternalRecordTopic { .. })));

    // A message topic over the external carrier completes the same handshake.
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], peer.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&peer, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(3),
        a.clone(),
        peer.clone(),
        Carrier::External,
        1_000,
        None,
        offer(&a, Direction::Both),
    )
    .unwrap();
    let (mut proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut external =
        Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let accept = external
        .accept(
            offer(&peer, Direction::Both),
            consent(&peer, "user:p", 1_300),
            1_300,
            nonce(2),
        )
        .unwrap();
    proposer.receive(&accept, 1_400).unwrap();
    let key_message = proposer.key_exchange(1_450, nonce(3)).unwrap();
    external.receive(&key_message, 1_500).unwrap();
    let key_back = external.key_exchange(1_550, nonce(4)).unwrap();
    proposer.receive(&key_back, 1_600).unwrap();
    let activate = proposer.activate(1_700, nonce(5)).unwrap();
    external.receive(&activate, 1_800).unwrap();
    let link = external.into_link();
    assert_eq!(link.state, LinkState::Active);
    assert_eq!(link.parties.iter().filter(|p| p.is_tenant()).count(), 1);
}

#[test]
fn a_rejection_ends_the_link_and_blocks_new_exchanges() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(4),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (mut proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let rejection = peer
        .reject(
            spot_tenant_link::RejectReason::TermsDeclined,
            1_300,
            nonce(2),
        )
        .unwrap();
    assert_eq!(
        proposer.receive(&rejection, 1_400).unwrap(),
        spot_tenant_link::Received::Rejected
    );
    let link = proposer.into_link();
    assert_eq!(link.state, LinkState::Revoked);
    assert!(link.revocation.is_some());
    assert!(matches!(
        link.allows(&topic(), Direction::Send, 1_500),
        Err(Error::Revoked { .. })
    ));
}

#[test]
fn one_actor_cannot_consent_for_both_parties() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(5),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (_proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:same", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let same_actor = consent(&b, "user:same", 1_300);
    assert!(matches!(
        peer.accept(offer(&b, Direction::Receive), same_actor, 1_300, nonce(2)),
        Err(Error::ConsentDuplicate { .. })
    ));
}

#[test]
fn a_tampered_message_is_refused() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(6),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (mut proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let accept = peer
        .accept(
            offer(&b, Direction::Receive),
            consent(&b, "user:b", 1_300),
            1_300,
            nonce(2),
        )
        .unwrap();
    let mut tampered = accept;
    match &mut tampered.body {
        spot_tenant_link::HandshakeBody::Accept(body) => {
            body.offer.grants[0].expires += 1;
        }
        _ => panic!("the fixture builds an acceptance"),
    }
    assert!(matches!(
        proposer.receive(&tampered, 1_400),
        Err(Error::BadSignature)
    ));
}

#[test]
fn a_valid_message_for_another_link_is_refused() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(7),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (mut proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let _peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    // Correctly signed, correct revision and transcript, but names another link.
    let mut message = spot_tenant_link::HandshakeMessage {
        revision: 1,
        previous: Some(propose.digest()),
        nonce: nonce(9),
        signer: key_b.public_key(),
        body: spot_tenant_link::HandshakeBody::Accept(spot_tenant_link::AcceptBody {
            link: link_id(8),
            offer: offer(&b, Direction::Receive),
            consent: consent(&b, "user:b", 1_300),
        }),
        signature: spot_tenant_link::SignatureBytes::from_bytes([0u8; 64]),
    };
    message.signature = key_b.sign(&message.signing_bytes());
    assert!(matches!(
        proposer.receive(&message, 1_400),
        Err(Error::WrongLink { .. })
    ));
}

#[test]
fn a_consent_from_the_wrong_party_is_refused() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(9),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (_proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let wrong_party: Consent = consent(&a, "user:b", 1_300);
    assert!(matches!(
        peer.accept(offer(&b, Direction::Receive), wrong_party, 1_300, nonce(2)),
        Err(Error::ConsentInvalid { .. })
    ));
    // A consent whose offer belongs to the other side is refused too.
    let wrong_offer = offer(&a, Direction::Receive);
    assert!(matches!(
        peer.accept(wrong_offer, consent(&b, "user:b", 1_300), 1_300, nonce(2)),
        Err(Error::InvalidOffer { .. })
    ));
}
