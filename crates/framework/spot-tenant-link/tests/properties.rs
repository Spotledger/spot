//! Hostile-input and property tests for the tenant link wire and handshake (WP-CTL-09.T9).
//!
//! A link is a trust relationship between two organisations, and the signed messages are the one
//! surface an outside party's bytes reach before anything is trusted. These properties say what
//! must hold whatever those bytes are:
//!
//! 1. arbitrary bytes and arbitrary JSON never decode into a message that verifies;
//! 2. a single flipped bit, or any one changed field with the old signature, is refused;
//! 3. replayed, reordered, duplicated and foreign messages never move a link to a state the
//!    transition table forbids, and a replay changes nothing;
//! 4. a revoked link refuses every use, its grants are untouched, and `after_revocation` maps
//!    every delivery state to its documented outcome;
//! 5. the decision of `Link::allows` and `allows_with_region` equals a straight-line reference
//!    model written here, not a call into the code under test;
//! 6. every error the properties provoke has a catalogue entry and carries no key or signature.
//!
//! Strategies and the signed-message fixture live in `common/generators.rs`. The cases are 256
//! for the fast properties and 64 for the interleavings; a failure prints the minimal input.
//! No failure-persistence file is written (a regression is pinned as an example test instead).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/generators.rs"]
mod generators;

use std::collections::BTreeSet;
use std::fmt::Debug;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use spot_data_policy::Region;
use spot_tenant_link::{
    DeliveryState, Digest, Direction, Error, ExternalPeerId, Handshake, HandshakeBody,
    HandshakeMessage, IdentitySigner, Link, LinkId, LinkService, LinkState, LinkStore,
    MemoryAuditLog, MemoryLinkStore, MemoryNonceLog, MemoryRegionOracle, Nonce, Party, PeerKey,
    RevocationOutcome, RevokeMessage, RevokeReason, SeedSigner, SignatureBytes, Topic,
    after_revocation, codes,
};

use common::{Ports, consent, nonce, offer, tenant};
use generators::*;

fn fast() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

fn slow() -> ProptestConfig {
    ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

// ---------------------------------------------------------------------------------------------
// The error vocabulary (property 6)
// ---------------------------------------------------------------------------------------------

#[test]
fn the_codes_module_and_the_catalogue_agree() {
    let unique: BTreeSet<&str> = ALL_CODES.iter().copied().collect();
    assert_eq!(unique.len(), ALL_CODES.len(), "a code is listed twice");
    for code in ALL_CODES {
        assert!(code.starts_with("tenant_link."), "{code}");
        assert!(catalogue().contains(code), "{code} has no catalogue entry");
    }
}

// ---------------------------------------------------------------------------------------------
// Property 1: garbage in, nothing authenticated
// ---------------------------------------------------------------------------------------------

/// Whether `bytes` decode into a handshake message whose signature verifies.
fn authenticates_handshake(bytes: &[u8]) -> bool {
    serde_json::from_slice::<HandshakeMessage>(bytes)
        .map(|message| message.verify().is_ok())
        .unwrap_or(false)
}

/// Whether `bytes` decode into a revoke message whose signature verifies.
fn authenticates_revoke(bytes: &[u8]) -> bool {
    serde_json::from_slice::<RevokeMessage>(bytes)
        .map(|message| message.verify().is_ok())
        .unwrap_or(false)
}

fn value_authenticates(value: &Value) -> bool {
    let handshake = serde_json::from_value::<HandshakeMessage>(value.clone())
        .map(|message| message.verify().is_ok())
        .unwrap_or(false);
    let revoke = serde_json::from_value::<RevokeMessage>(value.clone())
        .map(|message| message.verify().is_ok())
        .unwrap_or(false);
    handshake || revoke
}

/// A typed identifier decodes from text or is refused; what decodes survives a second round trip
/// unchanged. It never panics.
fn text_round_trips<T: Serialize + DeserializeOwned + PartialEq + Debug>(
    text: &str,
) -> Result<(), TestCaseError> {
    if let Ok(first) = serde_json::from_value::<T>(Value::String(text.to_owned())) {
        let again: T = serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
        prop_assert_eq!(first, again);
    }
    Ok(())
}

proptest! {
    #![proptest_config(fast())]

    #[test]
    fn arbitrary_bytes_never_authenticate(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        prop_assert!(!authenticates_handshake(&bytes));
        prop_assert!(!authenticates_revoke(&bytes));
    }

    #[test]
    fn arbitrary_json_never_authenticates(value in json_value()) {
        prop_assert!(!value_authenticates(&value));
    }

    #[test]
    fn json_shaped_like_a_message_never_authenticates(
        handshake in shaped_message(),
        revoke in shaped_revoke(),
    ) {
        prop_assert!(!value_authenticates(&handshake));
        prop_assert!(!value_authenticates(&revoke));
    }

    #[test]
    fn typed_identifiers_never_panic_and_round_trip(text in "\\PC{0,200}") {
        text_round_trips::<LinkId>(&text)?;
        text_round_trips::<Party>(&text)?;
        text_round_trips::<PeerKey>(&text)?;
        text_round_trips::<Digest>(&text)?;
        text_round_trips::<Nonce>(&text)?;
        text_round_trips::<SignatureBytes>(&text)?;
        text_round_trips::<Topic>(&text)?;
        text_round_trips::<ExternalPeerId>(&text)?;
    }

    #[test]
    fn hexadecimal_shaped_text_never_panics(
        key in generators::hex_text(64),
        signature in generators::hex_text(128),
        nonce_text in generators::hex_text(32),
    ) {
        text_round_trips::<PeerKey>(&key)?;
        text_round_trips::<Digest>(&key)?;
        text_round_trips::<SignatureBytes>(&signature)?;
        text_round_trips::<Nonce>(&nonce_text)?;
    }
}

// ---------------------------------------------------------------------------------------------
// Property 2: bit flips and field changes
// ---------------------------------------------------------------------------------------------

/// The five messages and one revoke of a handshake, as the JSON a transport would carry.
fn encoded(messages: &Messages, revoke: &RevokeMessage) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = messages
        .all()
        .iter()
        .map(|message| serde_json::to_vec(message).unwrap())
        .collect();
    out.push(serde_json::to_vec(revoke).unwrap());
    out
}

fn revoke_for(messages: &Messages, params: &Params) -> RevokeMessage {
    RevokeMessage::sign(
        &messages.signer_b(params),
        messages.id,
        RevokeReason::SecurityConcern,
        nonce(77),
    )
}

proptest! {
    #![proptest_config(fast())]

    /// Flip one bit of the encoded form: the decoder refuses it, or it decodes to something that
    /// does not verify, or it decodes to exactly the original (a hexadecimal digit changing case).
    #[test]
    fn a_single_flipped_bit_of_an_encoded_message_is_refused(
        p in params(),
        which in 0usize..6,
        position in any::<proptest::sample::Index>(),
        bit in 0u8..8,
    ) {
        let messages = build(&p);
        let revoke = revoke_for(&messages, &p);
        let mut bytes = encoded(&messages, &revoke).remove(which);
        let at = position.index(bytes.len());
        bytes[at] ^= 1 << bit;
        if which < 5 {
            let original = messages.all()[which].clone();
            let outcome = serde_json::from_slice::<HandshakeMessage>(&bytes);
            let tolerated = match outcome {
                Err(_) => true,
                Ok(decoded) => decoded == original || decoded.verify().is_err(),
            };
            prop_assert!(tolerated);
        } else {
            let outcome = serde_json::from_slice::<RevokeMessage>(&bytes);
            let tolerated = match outcome {
                Err(_) => true,
                Ok(decoded) => decoded == revoke || decoded.verify().is_err(),
            };
            prop_assert!(tolerated);
        }
    }

    /// Flip one bit of the signed bytes: the signature no longer verifies.
    #[test]
    fn a_flipped_bit_of_the_signed_bytes_fails_verification(
        p in params(),
        which in 0usize..6,
        position in any::<proptest::sample::Index>(),
        bit in 0u8..8,
    ) {
        let messages = build(&p);
        let revoke = revoke_for(&messages, &p);
        let (signer, mut signed, signature) = if which < 5 {
            let message = messages.all()[which];
            (message.signer, message.signing_bytes(), message.signature)
        } else {
            (revoke.signer, revoke.signing_bytes(), revoke.signature)
        };
        let at = position.index(signed.len());
        signed[at] ^= 1 << bit;
        prop_assert!(signer.verify(&signed, &signature).is_err());
    }

    /// Flip one bit of the signature itself.
    #[test]
    fn a_flipped_bit_of_the_signature_fails_verification(
        p in params(),
        which in 0usize..5,
        position in 0usize..64,
        bit in 0u8..8,
    ) {
        let messages = build(&p);
        let message = messages.all()[which].clone();
        let mut bytes = *message.signature.as_bytes();
        bytes[position] ^= 1 << bit;
        let mut tampered = message;
        tampered.signature = SignatureBytes::from_bytes(bytes);
        prop_assert!(tampered.verify().is_err());
    }

    /// A random signature over a real message never verifies.
    #[test]
    fn a_random_signature_never_verifies(
        p in params(),
        which in 0usize..5,
        random in any::<[u8; 32]>(),
        more in any::<[u8; 32]>(),
    ) {
        let messages = build(&p);
        let mut message = messages.all()[which].clone();
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&random);
        bytes[32..].copy_from_slice(&more);
        message.signature = SignatureBytes::from_bytes(bytes);
        prop_assert!(message.verify().is_err());
    }
}

proptest! {
    #![proptest_config(slow())]

    /// Every field the signature covers: change it, keep the old signature, and the message
    /// neither verifies nor signs the same bytes.
    #[test]
    fn every_signed_field_is_covered_by_the_signature(p in params()) {
        let messages = build(&p);
        for message in messages.all() {
            for (label, changed) in field_changes(message, &messages) {
                prop_assert_ne!(
                    changed.signing_bytes(),
                    message.signing_bytes(),
                    "the field {} is not part of the signed bytes", label
                );
                prop_assert!(
                    changed.verify().is_err(),
                    "a message with a changed {} still verifies", label
                );
            }
        }
    }

    /// The same for the revoke message: version, link, revision, reason, nonce and signer.
    #[test]
    fn every_field_of_a_revoke_is_covered_by_the_signature(p in params()) {
        let messages = build(&p);
        let revoke = revoke_for(&messages, &p);
        prop_assert!(revoke.verify().is_ok());
        let mut changes: Vec<(&str, RevokeMessage)> = Vec::new();
        let mut version = revoke.clone();
        version.version += 1;
        changes.push(("version", version));
        let mut link = revoke.clone();
        link.link = common::link_id(p.link_seed.wrapping_add(1));
        changes.push(("link", link));
        let mut revision = revoke.clone();
        revision.revision -= 1;
        changes.push(("revision", revision));
        let mut reason = revoke.clone();
        reason.reason = RevokeReason::Superseded;
        changes.push(("reason", reason));
        let mut nonce_changed = revoke.clone();
        nonce_changed.nonce = Nonce::from_bytes([251u8; 16]);
        changes.push(("nonce", nonce_changed));
        let mut signer = revoke.clone();
        signer.signer = messages.public_a;
        changes.push(("signer", signer));
        for (label, changed) in changes {
            prop_assert!(changed.verify().is_err(), "a revoke with a changed {} verifies", label);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Property 3: replay, reorder and foreign messages against one side of a handshake
// ---------------------------------------------------------------------------------------------

/// An active link needs both consents from distinct parties and actors, and both keys pinned.
fn check_active_link(link: &Link) -> Result<(), TestCaseError> {
    if link.state == LinkState::Active {
        prop_assert_eq!(link.consents.len(), 2);
        prop_assert_ne!(&link.consents[0].party, &link.consents[1].party);
        prop_assert_ne!(&link.consents[0].actor, &link.consents[1].actor);
        prop_assert!(link.peer_keys.iter().all(Option::is_some));
        prop_assert_ne!(link.peer_keys[0], link.peer_keys[1]);
        prop_assert!(!link.grants.is_empty());
    }
    Ok(())
}

/// Drives the peer through `ops`. With `exact`, the peer's own messages and the instants of the
/// receives are the ones `build` used (its accept at 1 300 under `nonce_base + 1`, its key
/// exchange at 1 550 under `nonce_base + 3`): the transcript head every later message of the
/// proposer names then matches, which an honest run needs. Without it every step takes a fresh
/// instant and nonce, and a genuine message of the proposer is refused for naming another head.
fn drive_peer(
    p: &Params,
    foreign_params: &Params,
    ops: &[PeerOp],
    exact: bool,
) -> Result<Link, TestCaseError> {
    let messages = build(p);
    let foreign = build(foreign_params);
    let secrets = secrets_of(p, &messages);
    let signer_a = messages.signer_a(p);
    let signer_b = messages.signer_b(p);
    let ports = Ports::pinned(&messages.a, signer_a.public_key());
    let policy = ports.policy();
    let audit = MemoryAuditLog::new();
    let mut peer =
        Handshake::from_proposal(&signer_b, &audit, &policy, &messages.propose, 1_200).unwrap();
    let mut applied: Vec<usize> = Vec::new();
    let mut previous = peer.link().clone();
    for (index, op) in ops.iter().enumerate() {
        let at = 2_000 + u64::try_from(index).unwrap();
        // The peer's own nonces must not coincide with the five the generated exchange uses
        // (`nonce_base` runs over every byte, so a small counter eventually collides with one):
        // the generated ones repeat one byte sixteen times, these never do.
        let mut fresh_bytes = [0xC3u8; 16];
        fresh_bytes[0] = u8::try_from(index).unwrap();
        fresh_bytes[15] = 0x5A;
        let mut fresh = Nonce::from_bytes(fresh_bytes);
        let mut at = at;
        if exact {
            (at, fresh) = match *op {
                PeerOp::Accept => (1_300, nonce(p.nonce_base.wrapping_add(1))),
                PeerOp::KeyExchange => (1_550, nonce(p.nonce_base.wrapping_add(3))),
                PeerOp::Receive(Delivery::Activate) => (1_800, fresh),
                PeerOp::Receive(_) => (1_500, fresh),
            };
        }
        let revision_before = peer.next_revision();
        let result: Result<(), Error> = match *op {
            PeerOp::Accept => peer
                .accept(
                    offer(&messages.b, Direction::Receive),
                    consent(&messages.b, "user:b", at),
                    at,
                    fresh,
                )
                .map(|_| ()),
            PeerOp::KeyExchange => peer.key_exchange(at, fresh).map(|_| ()),
            PeerOp::Receive(kind) => {
                let message = match kind {
                    Delivery::Propose => messages.propose.clone(),
                    Delivery::KeyA => messages.key_a.clone(),
                    Delivery::Activate => messages.activate.clone(),
                    Delivery::ForeignKeyA => foreign.key_a.clone(),
                    Delivery::ForeignActivate => foreign.activate.clone(),
                    Delivery::TamperedActivate => {
                        let mut tampered = messages.activate.clone();
                        if let HandshakeBody::Activate(body) = &mut tampered.body {
                            body.grants = Digest::from_bytes([7u8; 32]);
                        }
                        tampered
                    }
                    Delivery::WrongSignerKeyA => {
                        let mut wrong = messages.key_a.clone();
                        wrong.signature =
                            SignatureBytes::from_bytes(*foreign.key_a.signature.as_bytes());
                        wrong
                    }
                };
                let key = op_key(kind);
                let outcome = peer.receive(&message, at).map(|_| ());
                // A replay of a message the peer already accepted, a foreign message and a
                // tampered one are refused and change nothing.
                if applied.contains(&key) || !is_genuine(kind) {
                    prop_assert!(outcome.is_err(), "{:?} was accepted again", kind);
                    prop_assert_eq!(peer.link(), &previous, "{:?} changed the link", kind);
                    prop_assert_eq!(peer.next_revision(), revision_before);
                } else if outcome.is_ok() {
                    applied.push(key);
                }
                outcome
            }
        };
        if let Err(error) = &result {
            check_error(error, &secrets)?;
        }
        let now = peer.link().clone();
        prop_assert!(
            previous.state == now.state || previous.state.can_become(now.state),
            "the state moved from {} to {}",
            previous.state,
            now.state
        );
        prop_assert!(
            !matches!(previous.state, LinkState::Revoked | LinkState::Expired)
                || previous.state == now.state,
            "a finished link came back"
        );
        check_active_link(&now)?;
        prop_assert_eq!(&now.id, &messages.id);
        previous = now;
    }
    Ok(previous)
}

/// A small number per genuine message, so the replay check knows which one was applied.
fn op_key(kind: Delivery) -> usize {
    match kind {
        Delivery::Propose => 0,
        Delivery::KeyA => 1,
        Delivery::Activate => 2,
        Delivery::ForeignKeyA => 3,
        Delivery::ForeignActivate => 4,
        Delivery::TamperedActivate => 5,
        Delivery::WrongSignerKeyA => 6,
    }
}

proptest! {
    #![proptest_config(slow())]

    /// Any interleaving of the peer's own steps with real, replayed, duplicated, foreign and
    /// tampered messages leaves the link in a state the table allows.
    #[test]
    fn an_interleaving_of_handshake_messages_never_breaks_the_state_table(
        (p, foreign) in two_params(),
        ops in proptest::collection::vec(peer_op(), 0..24),
    ) {
        drive_peer(&p, &foreign, &ops, false)?;
    }

    /// The honest order, with every step delivered twice and a foreign message in between, ends
    /// active on the peer's side and equal to the proposer's view of the grants.
    #[test]
    fn the_honest_order_with_duplicates_still_activates(
        (p, foreign) in two_params(),
        duplicate_each in any::<bool>(),
    ) {
        let honest = [
            PeerOp::Accept,
            PeerOp::Receive(Delivery::KeyA),
            PeerOp::Receive(Delivery::ForeignKeyA),
            PeerOp::KeyExchange,
            PeerOp::Receive(Delivery::Activate),
        ];
        let mut ops = Vec::new();
        for op in honest {
            ops.push(op);
            if duplicate_each && matches!(op, PeerOp::Receive(delivery) if is_genuine(delivery)) {
                ops.push(op);
            }
        }
        let link = drive_peer(&p, &foreign, &ops, true)?;
        prop_assert_eq!(link.state, LinkState::Active);
        let messages = build(&p);
        prop_assert_eq!(link.grants, messages.link_b.grants);
    }
}

// ---------------------------------------------------------------------------------------------
// Property 3 (continued): remote and local revocation, expiry and use on an active link
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum ServiceOp {
    /// A genuine revoke from the peer, with a fresh nonce.
    RemoteRevoke(RevokeReason),
    /// A revoke signed by a key that is not the pinned one.
    RemoteRevokeStranger,
    /// A genuine revoke edited after signing.
    RemoteRevokeStale,
    /// The last genuine revoke sent again.
    RemoteRevokeReplay,
    /// This side revokes.
    LocalRevoke(RevokeReason),
    /// Expiry is run at an instant.
    Expire(u64),
    /// A use is authorised at an instant.
    Authorize(u8, u64),
}

fn revoke_reason() -> impl Strategy<Value = RevokeReason> {
    prop_oneof![
        Just(RevokeReason::TermsEnded),
        Just(RevokeReason::SecurityConcern),
        Just(RevokeReason::Superseded),
    ]
}

fn instant() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(50_000u64),
        Just(99_999u64),
        Just(100_000u64),
        Just(150_000u64)
    ]
}

fn service_op() -> impl Strategy<Value = ServiceOp> {
    prop_oneof![
        revoke_reason().prop_map(ServiceOp::RemoteRevoke),
        Just(ServiceOp::RemoteRevokeStranger),
        Just(ServiceOp::RemoteRevokeStale),
        Just(ServiceOp::RemoteRevokeReplay),
        revoke_reason().prop_map(ServiceOp::LocalRevoke),
        instant().prop_map(ServiceOp::Expire),
        (0u8..3, instant()).prop_map(|(d, t)| ServiceOp::Authorize(d, t)),
    ]
}

/// The grant of the fixture expires at 100 000, sends only, and the link has no expiry of its own.
const FIXTURE_EXPIRES: u64 = 100_000;

proptest! {
    #![proptest_config(slow())]

    /// Remote revokes (genuine, stranger, stale, replayed), local revokes, expiry and uses in any
    /// order: the stored link follows a three-state model, never leaves a finished state, only
    /// ever moves its version forward by one, and a replay or a stranger changes nothing.
    #[test]
    fn revocation_expiry_and_use_follow_the_state_model(
        p in params(),
        ops in proptest::collection::vec(service_op(), 0..20),
    ) {
        let messages = build(&p);
        let secrets = secrets_of(&p, &messages);
        let signer_b = messages.signer_b(&p);
        let stranger = SeedSigner::from_seed([p.seed_a[0] ^ 0x55; 32], messages.b.clone());
        let store = MemoryLinkStore::new();
        let audit = MemoryAuditLog::new();
        let nonces = MemoryNonceLog::new();
        let service = LinkService::new(&store, &audit);
        let original = store.insert(messages.link_a.clone()).unwrap();
        let id = original.id;
        let topic = Topic::new("bpm.message").unwrap();
        let mut state = LinkState::Active;
        let mut version = original.version;
        let mut last_genuine: Option<RevokeMessage> = None;
        let mut next_nonce = 10u8;
        for op in ops {
            let before = store.get(&id).unwrap();
            let outcome: Result<(), Error> = match op {
                ServiceOp::RemoteRevoke(reason) => {
                    next_nonce = next_nonce.wrapping_add(1);
                    let message = RevokeMessage::sign(&signer_b, id, reason, nonce(next_nonce));
                    last_genuine = Some(message.clone());
                    let result = service.receive_revoke(&message, &nonces, 3_000).map(|_| ());
                    if state == LinkState::Active {
                        prop_assert!(result.is_ok(), "{:?}", result);
                        state = LinkState::Revoked;
                        version += 1;
                        let stored = store.get(&id).unwrap();
                        prop_assert_eq!(stored.revocation.unwrap().by, messages.b.clone());
                    } else {
                        prop_assert!(result.is_err());
                    }
                    result
                }
                ServiceOp::RemoteRevokeStranger => {
                    next_nonce = next_nonce.wrapping_add(1);
                    let message = RevokeMessage::sign(&stranger, id, RevokeReason::TermsEnded, nonce(next_nonce));
                    let result = service.receive_revoke(&message, &nonces, 3_000).map(|_| ());
                    prop_assert!(result.is_err(), "a revoke from an unpinned key was accepted");
                    prop_assert!(!nonces.contains(&id, &nonce(next_nonce)).unwrap());
                    result
                }
                ServiceOp::RemoteRevokeStale => {
                    next_nonce = next_nonce.wrapping_add(1);
                    let mut message = RevokeMessage::sign(&signer_b, id, RevokeReason::TermsEnded, nonce(next_nonce));
                    message.revision = spot_tenant_link::REVOKE_ACTIVE_REVISION - 1;
                    let result = service.receive_revoke(&message, &nonces, 3_000).map(|_| ());
                    prop_assert!(result.is_err(), "an edited revoke was accepted");
                    result
                }
                ServiceOp::RemoteRevokeReplay => match &last_genuine {
                    None => Ok(()),
                    Some(message) => {
                        let result = service.receive_revoke(message, &nonces, 3_001).map(|_| ());
                        prop_assert!(result.is_err(), "a replayed revoke was accepted");
                        result
                    }
                },
                ServiceOp::LocalRevoke(reason) => {
                    let result = service.revoke(&id, reason, "user:a", 3_002).map(|_| ());
                    if state == LinkState::Active {
                        prop_assert!(result.is_ok(), "{:?}", result);
                        state = LinkState::Revoked;
                        version += 1;
                    } else {
                        prop_assert!(result.is_err());
                    }
                    result
                }
                ServiceOp::Expire(at) => {
                    let result = service.expire(&id, at).map(|_| ());
                    if state == LinkState::Active && at >= FIXTURE_EXPIRES {
                        prop_assert!(result.is_ok(), "{:?}", result);
                        state = LinkState::Expired;
                        version += 1;
                    } else {
                        prop_assert!(result.is_err());
                    }
                    result
                }
                ServiceOp::Authorize(direction, now) => {
                    let wanted = direction_of(direction);
                    let result = service
                        .authorize(&id, &topic, wanted, now, "user:a", None)
                        .map(|_| ());
                    let allowed = state == LinkState::Active
                        && now < FIXTURE_EXPIRES
                        && wanted == Direction::Send;
                    prop_assert_eq!(result.is_ok(), allowed);
                    result
                }
            };
            if let Err(error) = &outcome {
                check_error(error, &secrets)?;
            }
            let stored = store.get(&id).unwrap();
            prop_assert_eq!(stored.state, state);
            prop_assert_eq!(stored.version, version);
            prop_assert_eq!(&stored.grants, &original.grants);
            prop_assert_eq!(&stored.consents, &original.consents);
            prop_assert_eq!(&stored.peer_keys, &original.peer_keys);
            if outcome.is_err() {
                prop_assert_eq!(&stored, &before, "a refused operation changed the stored link");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Property 4: a revoked link refuses every use, grants stay as they were
// ---------------------------------------------------------------------------------------------

/// The documented outcome of revocation for each delivery state, restated here.
fn documented_outcome(state: DeliveryState) -> RevocationOutcome {
    match state {
        DeliveryState::Queued | DeliveryState::Sent => RevocationOutcome::HeldRevoked,
        DeliveryState::Acknowledged | DeliveryState::Applied | DeliveryState::Rejected => {
            RevocationOutcome::Stays
        }
        DeliveryState::Expired => RevocationOutcome::AlreadyExpired,
    }
}

#[test]
fn after_revocation_maps_every_delivery_state_to_its_documented_outcome() {
    for state in [
        DeliveryState::Queued,
        DeliveryState::Sent,
        DeliveryState::Acknowledged,
        DeliveryState::Applied,
        DeliveryState::Rejected,
        DeliveryState::Expired,
    ] {
        assert_eq!(
            after_revocation(state),
            documented_outcome(state),
            "{state:?}"
        );
    }
}

proptest! {
    #![proptest_config(fast())]

    /// Revoke in any order from either side: the first revocation wins and is recorded, later ones
    /// are refused, a stranger cannot revoke, the grants are untouched, and afterwards every use
    /// of every topic in every direction at every instant is refused as revoked.
    #[test]
    fn a_revoked_link_refuses_every_use_and_keeps_its_grants(
        specs in proptest::collection::vec(grant_spec(), 1..6),
        sequence in proptest::collection::vec((any::<bool>(), revoke_reason(), 1_000u64..300_000), 1..5),
        probes in proptest::collection::vec((0usize..TOPICS.len(), 0u8..3, 0u64..300_000), 1..12),
    ) {
        let link = link_with(&specs, None);
        prop_assume!(link.validate().is_ok());
        let grants_before = link.grants.clone();
        let sides = [link.parties[0].clone(), link.parties[1].clone()];
        let mut current = link.clone();
        let mut first: Option<(Party, RevokeReason, u64)> = None;
        for (use_first, reason, at) in &sequence {
            let by = if *use_first { sides[0].clone() } else { sides[1].clone() };
            match current.revoked_by(&by, *reason, *at) {
                Ok(next) => {
                    prop_assert!(first.is_none(), "a second revocation succeeded");
                    first = Some((by, *reason, *at));
                    current = next;
                }
                Err(error) => {
                    prop_assert!(first.is_some(), "the first revocation was refused: {error}");
                    prop_assert_eq!(error.code(), codes::INVALID_TRANSITION);
                }
            }
        }
        let (by, reason, at) = first.unwrap();
        let recorded = current.revocation.clone().unwrap();
        prop_assert_eq!((recorded.by, recorded.reason, recorded.at), (by, reason, at));
        prop_assert_eq!(&current.grants, &grants_before);
        prop_assert!(link.revoked_by(&tenant(120), RevokeReason::TermsEnded, 5).is_err());
        for (topic, direction, now) in probes {
            let topic = Topic::new(TOPICS[topic]).unwrap();
            let wanted = direction_of(direction);
            let plain = current.allows(&topic, wanted, now).map(|_| ());
            prop_assert_eq!(plain.unwrap_err().code(), codes::REVOKED);
            let oracle = MemoryRegionOracle::new();
            let with_region = current.allows_with_region(&topic, wanted, now, &oracle).map(|_| ());
            prop_assert_eq!(with_region.unwrap_err().code(), codes::REVOKED);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Property 5: region and expiry against a reference model
// ---------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(fast())]

    /// Random grants, a random link expiry, an instant chosen around every expiry (one before,
    /// at, one after), a direction and a region answer: the crate decides as the model does.
    #[test]
    fn the_decision_equals_the_reference_model(
        specs in proptest::collection::vec(grant_spec(), 1..6),
        link_expires in proptest::option::of(2_000u64..200_000),
        topic in 0usize..TOPICS.len(),
        direction in 0u8..3,
        selector in any::<proptest::sample::Index>(),
        answer in answer_strategy(),
    ) {
        let link = link_with(&specs, link_expires);
        prop_assume!(link.validate().is_ok());
        let mut instants: Vec<u64> = vec![0, 1_000, 1_999];
        for grant in &link.grants {
            instants.extend([grant.expires - 1, grant.expires, grant.expires + 1]);
        }
        if let Some(expires) = link.expires {
            instants.extend([expires - 1, expires, expires + 1]);
        }
        let now = instants[selector.index(instants.len())];
        let topic = Topic::new(TOPICS[topic]).unwrap();
        let wanted = direction_of(direction);
        let decided = match &answer {
            Answer::NoOracle => link.allows(&topic, wanted, now).map(|_| ()),
            Answer::Unknown => {
                let oracle = MemoryRegionOracle::new();
                link.allows_with_region(&topic, wanted, now, &oracle).map(|_| ())
            }
            Answer::Refuses => link.allows_with_region(&topic, wanted, now, &Refusing).map(|_| ()),
            Answer::Region(found) => {
                let oracle = MemoryRegionOracle::new();
                oracle
                    .insert(link.peer().unwrap().clone(), Region::new(generators::REGIONS[*found]).unwrap())
                    .unwrap();
                link.allows_with_region(&topic, wanted, now, &oracle).map(|_| ())
            }
        };
        let expected = reference(&link, &topic, wanted, now, &answer);
        prop_assert_eq!(decided.as_ref().err().map(Error::code), expected);
        if let Err(error) = &decided {
            check_error(error, &[])?;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Property 6: what an error says
// ---------------------------------------------------------------------------------------------

#[test]
fn error_texts_for_the_wire_refusals_carry_no_key_or_signature() {
    // A wrong signer and a mismatched key name the keys by fingerprint only.
    let params = Params {
        link_seed: 1,
        tenant_a: 1,
        tenant_b: 51,
        seed_a: [11u8; 32],
        seed_b: [12u8; 32],
        nonce_base: 10,
    };
    let messages = build(&params);
    let secrets = secrets_of(&params, &messages);
    let signer_a = messages.signer_a(&params);
    let signer_b = messages.signer_b(&params);
    let ports = Ports::pinned(&messages.a, signer_a.public_key());
    let policy = ports.policy();
    let audit = MemoryAuditLog::new();
    let mut peer =
        Handshake::from_proposal(&signer_b, &audit, &policy, &messages.propose, 1_200).unwrap();
    let mut wrong = messages.propose.clone();
    wrong.signer = messages.public_b;
    let refused = [
        peer.receive(&wrong, 1_300).unwrap_err(),
        peer.receive(&messages.propose, 1_300).unwrap_err(),
        peer.receive(&messages.activate, 1_300).unwrap_err(),
    ];
    for error in &refused {
        let shown = format!("{error} {error:?}").to_lowercase();
        for secret in &secrets {
            assert!(!shown.contains(secret), "{}: {shown}", error.code());
        }
        assert!(catalogue().contains(error.code()), "{}", error.code());
    }
}

/// A consent actor is peer-supplied text, and three errors repeat it. The brief for this suite
/// asks that no error text echo peer-supplied free text; this property pins the finding and is
/// The errors carry a fingerprint of the actor, never the text.
#[test]
fn no_error_text_echoes_a_peer_supplied_consent_actor() {
    const CANARY: &str = "user:CANARY-FREE-TEXT-0451";
    let params = Params {
        link_seed: 2,
        tenant_a: 2,
        tenant_b: 52,
        seed_a: [21u8; 32],
        seed_b: [22u8; 32],
        nonce_base: 20,
    };
    let a = tenant(params.tenant_a);
    let b = tenant(params.tenant_b);
    let key_a = SeedSigner::from_seed(params.seed_a, a.clone());
    let key_b = SeedSigner::from_seed(params.seed_b, b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = spot_tenant_link::LinkDraft::new(
        common::link_id(2),
        a.clone(),
        b.clone(),
        spot_tenant_link::Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    // The proposer's consent is fresh when made and stale when the peer uses it: the peer's
    // refusal says who the (peer-supplied) actor was.
    let (_, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, CANARY, 1_100),
        1_100,
        nonce(20),
    )
    .unwrap();
    let late = 1_100 + 16 * 60 * 1_000;
    let error = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, late)
        .err()
        .unwrap();
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains("CANARY-FREE-TEXT-0451"), "{shown}");
}

#[test]
fn offer_and_party_types_never_echo_a_hostile_topic_in_a_decode_error() {
    // Decoding refuses with a fixed reason, never with the offending text.
    for hostile in ["CANARY-TOPIC", "bpm.CANARY", "bpm", "bpm..x", ""] {
        let error = Topic::new(hostile).unwrap_err();
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("CANARY"), "{shown}");
        assert!(catalogue().contains(error.code()));
    }
    for hostile in [
        "CANARY",
        "tenant:CANARY",
        "external:CANARY",
        "external:Canary",
    ] {
        let error = Party::try_from(hostile.to_owned()).unwrap_err();
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("CANARY") && !shown.contains("Canary"),
            "{shown}"
        );
    }
}
