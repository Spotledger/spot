//! Strategies and signed-message fixtures for the property suite (WP-CTL-09.T9).
//!
//! Nothing here reads a clock or a random source: every key seed, nonce and link id comes from
//! the proptest case, so a failing case replays exactly.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::OnceLock;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use serde_json::Value;
use spot_data_policy::{Clause, Refusal, Region};
use spot_iam_access::{AuthMethod, OperationName};
use spot_tenant_link::{
    Consent, Digest, Direction, Error, FieldList, FieldName, Grant, Handshake, HandshakeBody,
    HandshakeMessage, IdentitySigner, Link, LinkDraft, LinkId, LinkState, MemoryAuditLog, Nonce,
    Party, PeerKey, RegionOracle, SchemaRange, SeedSigner, Topic, TopicKind, codes,
};

use crate::common::{Ports, consent, link_id, nonce, offer, tenant};

/// Everything that distinguishes one complete handshake from another.
#[derive(Clone, Debug)]
pub struct Params {
    /// Distinguishes the link id (1 to 200; the tests use 250 and up for changed ids).
    pub link_seed: u8,
    /// Tenant number of the proposing side (1 to 50).
    pub tenant_a: u8,
    /// Tenant number of the accepting side (51 to 90; the tests use 91 and up for strangers).
    pub tenant_b: u8,
    /// The proposing side's identity seed.
    pub seed_a: [u8; 32],
    /// The accepting side's identity seed.
    pub seed_b: [u8; 32],
    /// The first of the five nonces the exchange uses (consecutive values).
    pub nonce_base: u8,
}

/// One case's parameters. The two seeds always differ (a link needs two different keys).
pub fn params() -> impl Strategy<Value = Params> {
    (
        1u8..=200,
        1u8..=50,
        51u8..=90,
        any::<[u8; 32]>(),
        any::<[u8; 32]>(),
        1u8..=120,
    )
        .prop_filter("the two identity seeds differ", |t| t.3 != t.4)
        .prop_map(
            |(link_seed, tenant_a, tenant_b, seed_a, seed_b, nonce_base)| Params {
                link_seed,
                tenant_a,
                tenant_b,
                seed_a,
                seed_b,
                nonce_base,
            },
        )
}

/// Two parameter sets that name different links (a second, unrelated exchange).
pub fn two_params() -> impl Strategy<Value = (Params, Params)> {
    (params(), params()).prop_filter("the links differ", |(a, b)| a.link_seed != b.link_seed)
}

/// One complete direct handshake, as the messages that crossed the wire and the two final links.
pub struct Messages {
    /// The proposing side.
    pub a: Party,
    /// The accepting side.
    pub b: Party,
    /// The link id.
    pub id: LinkId,
    /// Revision 0, proposer to peer.
    pub propose: HandshakeMessage,
    /// Revision 1, peer to proposer.
    pub accept: HandshakeMessage,
    /// Revision 2, proposer's key.
    pub key_a: HandshakeMessage,
    /// Revision 3, peer's key.
    pub key_b: HandshakeMessage,
    /// Revision 4, proposer to peer.
    pub activate: HandshakeMessage,
    /// The proposer's key.
    pub public_a: PeerKey,
    /// The peer's key.
    pub public_b: PeerKey,
    /// The proposer's final copy of the link.
    pub link_a: Link,
    /// The peer's final copy of the link.
    pub link_b: Link,
}

impl Messages {
    /// The five handshake messages in wire order.
    pub fn all(&self) -> [&HandshakeMessage; 5] {
        [
            &self.propose,
            &self.accept,
            &self.key_a,
            &self.key_b,
            &self.activate,
        ]
    }

    /// The proposer's signer.
    pub fn signer_a(&self, params: &Params) -> SeedSigner {
        SeedSigner::from_seed(params.seed_a, self.a.clone())
    }

    /// The peer's signer.
    pub fn signer_b(&self, params: &Params) -> SeedSigner {
        SeedSigner::from_seed(params.seed_b, self.b.clone())
    }
}

/// Runs a complete handshake between two tenants (A sends, B receives) and keeps every message.
pub fn build(params: &Params) -> Messages {
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
    let id = link_id(params.link_seed);
    let n = params.nonce_base;
    let draft = LinkDraft::new(
        id,
        a.clone(),
        b.clone(),
        spot_tenant_link::Carrier::Direct,
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
        nonce(n),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let accept = peer
        .accept(
            offer(&b, Direction::Receive),
            consent(&b, "user:b", 1_300),
            1_300,
            nonce(n + 1),
        )
        .unwrap();
    proposer.receive(&accept, 1_400).unwrap();
    let key_message_a = proposer.key_exchange(1_450, nonce(n + 2)).unwrap();
    peer.receive(&key_message_a, 1_500).unwrap();
    let key_message_b = peer.key_exchange(1_550, nonce(n + 3)).unwrap();
    proposer.receive(&key_message_b, 1_600).unwrap();
    let activate = proposer.activate(1_700, nonce(n + 4)).unwrap();
    peer.receive(&activate, 1_800).unwrap();
    Messages {
        a,
        b,
        id,
        propose,
        accept,
        key_a: key_message_a,
        key_b: key_message_b,
        activate,
        public_a: key_a.public_key(),
        public_b: key_b.public_key(),
        link_a: proposer.into_link(),
        link_b: peer.into_link(),
    }
}

// ---------------------------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------------------------

/// A strategy for lower-case hexadecimal text of exactly `len` characters.
pub fn hex_text(len: usize) -> BoxedStrategy<String> {
    proptest::string::string_regex(&format!("[0-9a-f]{{{len}}}"))
        .unwrap()
        .boxed()
}

/// Arbitrary JSON without floats: nulls, booleans, integers, strings (plain and hexadecimal of
/// the lengths the wire uses), arrays and objects, three levels deep.
pub fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<u64>().prop_map(|n| serde_json::json!(n)),
        "[ -~]{0,24}".prop_map(Value::String),
        hex_text(32).prop_map(Value::String),
        hex_text(64).prop_map(Value::String),
        hex_text(128).prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 32, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::hash_map("[a-z_]{1,10}", inner, 0..5)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

/// A JSON object shaped like a handshake message (the right keys, the right lengths of
/// hexadecimal) around a random body: it gets past the first look of a decoder.
pub fn shaped_message() -> impl Strategy<Value = Value> {
    (
        any::<u64>(),
        prop_oneof![Just(Value::Null), hex_text(64).prop_map(Value::String)],
        hex_text(32),
        hex_text(64),
        json_value(),
        hex_text(128),
    )
        .prop_map(|(revision, previous, nonce, signer, body, signature)| {
            serde_json::json!({
                "revision": revision,
                "previous": previous,
                "nonce": nonce,
                "signer": signer,
                "body": body,
                "signature": signature,
            })
        })
}

/// A JSON object shaped like a revoke message around random values.
pub fn shaped_revoke() -> impl Strategy<Value = Value> {
    (
        any::<u8>(),
        any::<u64>(),
        prop_oneof![
            Just("terms_ended".to_owned()),
            Just("security_concern".to_owned()),
            Just("superseded".to_owned()),
            "[ -~]{0,12}",
        ],
        hex_text(32),
        hex_text(64),
        hex_text(128),
        "[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
    )
        .prop_map(
            |(version, revision, reason, nonce, signer, signature, link)| {
                serde_json::json!({
                    "version": version,
                    "link": link,
                    "revision": revision,
                    "reason": reason,
                    "nonce": nonce,
                    "signer": signer,
                    "signature": signature,
                })
            },
        )
}

// ---------------------------------------------------------------------------------------------
// Links with random grants
// ---------------------------------------------------------------------------------------------

/// Six topics: some of them granted by a generated link, some not.
pub const TOPICS: [&str; 6] = [
    "bpm.alpha",
    "bpm.beta",
    "bpm.gamma",
    "bpm.delta",
    "bpm.epsilon",
    "bpm.zeta",
];

/// Three regions a grant may require.
pub const REGIONS: [&str; 3] = ["eu-west", "us-east", "ap-south"];

/// A grant, as small numbers a strategy can shrink.
#[derive(Clone, Debug)]
pub struct GrantSpec {
    /// Index into [`TOPICS`].
    pub topic: usize,
    /// 0 send, 1 receive, 2 both.
    pub direction: u8,
    /// The grant's expiry, Unix milliseconds.
    pub expires: u64,
    /// Index into [`REGIONS`] of the required peer region, if any.
    pub region: Option<usize>,
}

/// One grant specification.
pub fn grant_spec() -> impl Strategy<Value = GrantSpec> {
    (
        0usize..TOPICS.len(),
        0u8..3,
        2_000u64..200_000,
        proptest::option::of(0usize..REGIONS.len()),
    )
        .prop_map(|(topic, direction, expires, region)| GrantSpec {
            topic,
            direction,
            expires,
            region,
        })
}

/// The direction a spec names.
pub fn direction_of(code: u8) -> Direction {
    match code {
        0 => Direction::Send,
        1 => Direction::Receive,
        _ => Direction::Both,
    }
}

/// The grant a spec describes.
pub fn grant_of(spec: &GrantSpec) -> Grant {
    Grant {
        topic: Topic::new(TOPICS[spec.topic]).unwrap(),
        kind: TopicKind::Message,
        direction: direction_of(spec.direction),
        schema_range: SchemaRange::new(1, 2).unwrap(),
        fields: FieldList::only([FieldName::new("message.body").unwrap()]),
        expires: spec.expires,
        require_peer_region: spec.region.map(|i| Region::new(REGIONS[i]).unwrap()),
        notify: true,
    }
}

/// An active link (A's copy of a real handshake) whose grants and expiry are replaced. A topic
/// that appears twice in `specs` keeps its first grant.
pub fn link_with(specs: &[GrantSpec], link_expires: Option<u64>) -> Link {
    let mut link = crate::common::linked().a;
    let mut seen = Vec::new();
    link.grants = specs
        .iter()
        .filter(|spec| {
            let fresh = !seen.contains(&spec.topic);
            seen.push(spec.topic);
            fresh
        })
        .map(grant_of)
        .collect();
    link.expires = link_expires;
    link
}

// ---------------------------------------------------------------------------------------------
// Shared helpers of the property suite: field changes, delivery scripts, the reference model
// ---------------------------------------------------------------------------------------------

/// Every code the crate's `codes` module defines.
pub const ALL_CODES: [&str; 40] = [
    codes::INVALID_ID,
    codes::INVALID_PARTY,
    codes::INVALID_TOPIC,
    codes::INVALID_GRANT,
    codes::INVALID_SCHEMA_RANGE,
    codes::INVALID_FIELD,
    codes::FIELD_CLASSIFICATION,
    codes::INVALID_OFFER,
    codes::INVALID_EXPIRY,
    codes::EXTERNAL_RECORD_TOPIC,
    codes::NOT_FOUND,
    codes::VERSION_CONFLICT,
    codes::DUPLICATE_LINK,
    codes::INVALID_TRANSITION,
    codes::NOT_ACTIVE,
    codes::REVOKED,
    codes::LINK_EXPIRED,
    codes::GRANT_EXPIRED,
    codes::TOPIC_NOT_GRANTED,
    codes::WRONG_DIRECTION,
    codes::OFFER_CONFLICT,
    codes::GRANT_WIDER,
    codes::REGION_REFUSED,
    codes::PEER_REGION_MISMATCH,
    codes::BAD_SIGNATURE,
    codes::WRONG_SIGNER,
    codes::KEY_MISMATCH,
    codes::KEY_UNPINNED,
    codes::REPLAYED,
    codes::OUT_OF_ORDER,
    codes::TRANSCRIPT_MISMATCH,
    codes::WRONG_STEP,
    codes::WRONG_LINK,
    codes::CONSENT_REQUIRED,
    codes::CONSENT_DUPLICATE,
    codes::CONSENT_INVALID,
    codes::STEP_UP_REQUIRED,
    codes::CONSENT_STALE,
    codes::AUDIT_FAILED,
    codes::STORE_FAILED,
];

/// The `tenant_link.*` codes of `crates/framework/spot-errors/catalogue/`, read from the files.
pub fn catalogue() -> &'static BTreeSet<String> {
    static CODES: OnceLock<BTreeSet<String>> = OnceLock::new();
    CODES.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("spot-errors")
            .join("catalogue");
        let mut found = BTreeSet::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("code = \"")
                    && let Some(code) = rest.strip_suffix('"')
                    && code.starts_with("tenant_link.")
                {
                    found.insert(code.to_owned());
                }
            }
        }
        found
    })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What an error text must never contain: the identity seeds, the public keys and every
/// signature that took part.
pub fn secrets_of(params: &Params, messages: &Messages) -> Vec<String> {
    let mut secrets = vec![
        hex(&params.seed_a),
        hex(&params.seed_b),
        hex(messages.public_a.as_bytes()),
        hex(messages.public_b.as_bytes()),
    ];
    for message in messages.all() {
        secrets.push(hex(message.signature.as_bytes()));
    }
    secrets
}

/// Property 6 for one error: a stable code with a catalogue entry, and no key or signature in
/// its text.
pub fn check_error(error: &Error, secrets: &[String]) -> Result<(), TestCaseError> {
    prop_assert!(
        ALL_CODES.contains(&error.code()),
        "{} is not in the codes module",
        error.code()
    );
    prop_assert!(
        catalogue().contains(error.code()),
        "{} has no catalogue entry",
        error.code()
    );
    let shown = format!("{error} {error:?}").to_lowercase();
    for secret in secrets {
        prop_assert!(
            !shown.contains(secret),
            "an error text contains key or signature bytes: {}",
            error.code()
        );
    }
    Ok(())
}

/// A key that is not the one in the message: the other side's key, or a third one.
pub fn other_key(messages: &Messages, message: &HandshakeMessage) -> PeerKey {
    if message.signer == messages.public_a {
        messages.public_b
    } else {
        messages.public_a
    }
}

pub const GRANT_CHANGES: usize = 8;
pub const CONSENT_CHANGES: usize = 6;

/// Changes one field of a grant, to a value it did not have.
pub fn change_grant(grant: &mut Grant, change: usize) {
    match change {
        0 => grant.topic = Topic::new("bpm.other").unwrap(),
        1 => grant.kind = TopicKind::Record,
        2 => grant.direction = Direction::Both,
        3 => grant.schema_range = SchemaRange::new(1, 3).unwrap(),
        4 => grant.fields = FieldList::only([FieldName::new("message.other").unwrap()]),
        5 => grant.expires += 1,
        6 => grant.require_peer_region = Some(Region::new("eu-west").unwrap()),
        _ => grant.notify = !grant.notify,
    }
}

/// Changes one field of a consent, to a value it did not have.
pub fn change_consent(consent: &mut Consent, change: usize) {
    match change {
        0 => consent.party = tenant(99),
        1 => consent.actor = "user:someone-else".to_owned(),
        2 => consent.at += 1,
        3 => consent.proof.method = AuthMethod::Hardware,
        4 => consent.proof.authenticated_at += 1,
        _ => consent.proof.operation = OperationName::new("link.other"),
    }
}

/// Every single-field change of `message`, each re-encoded **without** re-signing: the label of
/// the field and the changed message.
pub fn field_changes(
    message: &HandshakeMessage,
    messages: &Messages,
) -> Vec<(String, HandshakeMessage)> {
    let mut out: Vec<(String, HandshakeMessage)> = Vec::new();
    let mut add = |label: String, edit: &dyn Fn(&mut HandshakeMessage)| {
        let mut changed = message.clone();
        edit(&mut changed);
        out.push((label, changed));
    };
    add("revision".to_owned(), &|m| m.revision += 1);
    add("previous".to_owned(), &|m| {
        m.previous = match m.previous {
            Some(_) => None,
            None => Some(Digest::from_bytes([5u8; 32])),
        };
    });
    add("nonce".to_owned(), &|m| {
        m.nonce = Nonce::from_bytes([250u8; 16])
    });
    let key = other_key(messages, message);
    add("signer".to_owned(), &|m| m.signer = key);
    match &message.body {
        HandshakeBody::Propose(_) => {
            add("draft.id".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.id = link_id(250);
                }
            });
            add("draft.local".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.local = tenant(99);
                }
            });
            add("draft.peer".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.peer = tenant(98);
                }
            });
            add("draft.carrier".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.carrier = spot_tenant_link::Carrier::Pull;
                }
            });
            add("draft.created".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.created += 1;
                }
            });
            add("draft.expires".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.expires = match b.draft.expires {
                        Some(_) => None,
                        None => Some(9_999_999),
                    };
                }
            });
            add("draft.offer.side".to_owned(), &|m| {
                if let HandshakeBody::Propose(b) = &mut m.body {
                    b.draft.offer.side = tenant(97);
                }
            });
            for change in 0..GRANT_CHANGES {
                add(format!("draft.offer.grants[0] change {change}"), &|m| {
                    if let HandshakeBody::Propose(b) = &mut m.body {
                        change_grant(&mut b.draft.offer.grants[0], change);
                    }
                });
            }
            for change in 0..CONSENT_CHANGES {
                add(format!("consent change {change}"), &|m| {
                    if let HandshakeBody::Propose(b) = &mut m.body {
                        change_consent(&mut b.consent, change);
                    }
                });
            }
        }
        HandshakeBody::Accept(_) => {
            add("link".to_owned(), &|m| {
                if let HandshakeBody::Accept(b) = &mut m.body {
                    b.link = link_id(251);
                }
            });
            add("offer.side".to_owned(), &|m| {
                if let HandshakeBody::Accept(b) = &mut m.body {
                    b.offer.side = tenant(96);
                }
            });
            for change in 0..GRANT_CHANGES {
                add(format!("offer.grants[0] change {change}"), &|m| {
                    if let HandshakeBody::Accept(b) = &mut m.body {
                        change_grant(&mut b.offer.grants[0], change);
                    }
                });
            }
            for change in 0..CONSENT_CHANGES {
                add(format!("consent change {change}"), &|m| {
                    if let HandshakeBody::Accept(b) = &mut m.body {
                        change_consent(&mut b.consent, change);
                    }
                });
            }
        }
        HandshakeBody::Reject(_) => {
            add("link".to_owned(), &|m| {
                if let HandshakeBody::Reject(b) = &mut m.body {
                    b.link = link_id(252);
                }
            });
            add("reason".to_owned(), &|m| {
                if let HandshakeBody::Reject(b) = &mut m.body {
                    b.reason = spot_tenant_link::RejectReason::KeysUnacceptable;
                }
            });
        }
        HandshakeBody::KeyExchange(_) => {
            add("link".to_owned(), &|m| {
                if let HandshakeBody::KeyExchange(b) = &mut m.body {
                    b.link = link_id(253);
                }
            });
            add("key".to_owned(), &|m| {
                if let HandshakeBody::KeyExchange(b) = &mut m.body {
                    b.key = key;
                }
            });
        }
        HandshakeBody::Activate(_) => {
            add("link".to_owned(), &|m| {
                if let HandshakeBody::Activate(b) = &mut m.body {
                    b.link = link_id(254);
                }
            });
            add("grants digest".to_owned(), &|m| {
                if let HandshakeBody::Activate(b) = &mut m.body {
                    b.grants = Digest::from_bytes([9u8; 32]);
                }
            });
        }
    }
    out
}

/// What the other side can deliver to the accepting peer.
#[derive(Clone, Copy, Debug)]
pub enum Delivery {
    /// The proposal again (the peer already consumed it to start).
    Propose,
    /// The proposer's key message.
    KeyA,
    /// The activation.
    Activate,
    /// A message of an unrelated link, valid for its own parties.
    ForeignKeyA,
    /// The activation of an unrelated link.
    ForeignActivate,
    /// The activation with its digest changed after signing.
    TamperedActivate,
    /// The proposer's key message with the signature of another key.
    WrongSignerKeyA,
}

#[derive(Clone, Copy, Debug)]
pub enum PeerOp {
    /// The peer sends its acceptance.
    Accept,
    /// The peer sends its key.
    KeyExchange,
    /// A message arrives.
    Receive(Delivery),
}

pub fn delivery() -> impl Strategy<Value = Delivery> {
    prop_oneof![
        Just(Delivery::Propose),
        Just(Delivery::KeyA),
        Just(Delivery::Activate),
        Just(Delivery::ForeignKeyA),
        Just(Delivery::ForeignActivate),
        Just(Delivery::TamperedActivate),
        Just(Delivery::WrongSignerKeyA),
    ]
}

pub fn peer_op() -> impl Strategy<Value = PeerOp> {
    prop_oneof![
        Just(PeerOp::Accept),
        Just(PeerOp::KeyExchange),
        delivery().prop_map(PeerOp::Receive),
        delivery().prop_map(PeerOp::Receive),
    ]
}

/// Whether the delivery is a message the peer may legitimately accept once.
pub fn is_genuine(delivery: Delivery) -> bool {
    matches!(delivery, Delivery::KeyA | Delivery::Activate)
}

/// How the peer's region is answered.
#[derive(Clone, Debug)]
pub enum Answer {
    /// `allows` with no oracle at all.
    NoOracle,
    /// An oracle that does not know the peer.
    Unknown,
    /// An oracle whose policy refuses.
    Refuses,
    /// An oracle that reports this region (an index into the region list).
    Region(usize),
}

pub struct Refusing;

impl RegionOracle for Refusing {
    fn peer_region(&self, _peer: &Party) -> Result<Option<Region>, Refusal> {
        Err(Refusal {
            clause: Clause::UnknownResource,
            region: Region::new("eu-west").unwrap(),
        })
    }
}

pub fn answer_strategy() -> impl Strategy<Value = Answer> {
    prop_oneof![
        Just(Answer::NoOracle),
        Just(Answer::Unknown),
        Just(Answer::Refuses),
        (0usize..REGIONS.len()).prop_map(Answer::Region),
    ]
}

/// The bit set of a direction: send 1, receive 2.
pub fn flows(direction: Direction) -> u8 {
    match direction {
        Direction::Send => 1,
        Direction::Receive => 2,
        Direction::Both => 3,
    }
}

/// The reference model: the code of the refusal, or `None` for allowed. It restates the rules of
/// the crate documentation (revoked first, then expiry, then the grant, its expiry, its direction,
/// then the peer's region) and shares no code with `Link`.
pub fn reference(
    link: &Link,
    topic: &Topic,
    wanted: Direction,
    now: u64,
    answer: &Answer,
) -> Option<&'static str> {
    if link.state == LinkState::Revoked {
        return Some(codes::REVOKED);
    }
    let link_over = link.expires.is_some_and(|expires| expires <= now);
    let every_grant_over =
        !link.grants.is_empty() && link.grants.iter().all(|grant| grant.expires <= now);
    if link_over || (link.state == LinkState::Active && every_grant_over) {
        return Some(codes::LINK_EXPIRED);
    }
    let Some(grant) = link.grants.iter().find(|grant| &grant.topic == topic) else {
        return Some(codes::TOPIC_NOT_GRANTED);
    };
    if grant.expires <= now {
        return Some(codes::GRANT_EXPIRED);
    }
    if flows(grant.direction) & flows(wanted) != flows(wanted) {
        return Some(codes::WRONG_DIRECTION);
    }
    let required = grant.require_peer_region.as_ref()?;
    match answer {
        Answer::NoOracle => Some(codes::PEER_REGION_MISMATCH),
        Answer::Unknown | Answer::Refuses => Some(codes::REGION_REFUSED),
        Answer::Region(found) => {
            if REGIONS[*found] == required.as_str() {
                None
            } else {
                Some(codes::PEER_REGION_MISMATCH)
            }
        }
    }
}
