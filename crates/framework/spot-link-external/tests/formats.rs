//! The peer's own message shape, through a mapping, in both directions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use common::*;
use spot_connectors::{Coercion, MappingRule, MappingSpec};
use spot_link_exchange::MessageState;
use spot_link_external::{Outbound, PeerFormat, codes};
use spot_tenant_link::Direction;

fn rule(target: &str, source: &str) -> MappingRule {
    MappingRule {
        target: target.to_owned(),
        source: source.to_owned(),
        expression: String::new(),
        coerce: Coercion::None,
    }
}

/// One mapping both ways would not fit both shapes, so the peer's mapping is the inbound one
/// (peer shape -> envelope) and the outbound one is a second spec on a second peer value.
fn inbound_mapping() -> MappingSpec {
    MappingSpec {
        rules: vec![
            rule("id", "orderRef"),
            rule("conversation", "thread"),
            rule("topic", "kind"),
            rule("schema_version", "version"),
            rule("payload.message.body", "text"),
        ],
    }
}

fn outbound_mapping() -> MappingSpec {
    MappingSpec {
        rules: vec![
            rule("orderRef", "id"),
            rule("thread", "conversation"),
            rule("kind", "topic"),
            rule("text", "payload.message.body"),
            rule("sequence", "seq"),
        ],
    }
}

#[tokio::test]
async fn a_mapped_peer_round_trips_a_message_both_ways() {
    // Outbound: the peer's shape on the wire.
    let mut world = World::new();
    world.peer.format = PeerFormat::Mapped(outbound_mapping());
    let id = world.enqueue("order-1", "ship it", T0);
    let peer = FakePeer::answering(200);
    assert_eq!(world.pump(&peer, T0 + 10).await.acknowledged, 1);
    assert_eq!(world.state_of(&id), MessageState::Acknowledged);
    let wire: serde_json::Value = serde_json::from_str(&peer.bodies()[0]).unwrap();
    assert_eq!(wire["orderRef"], id.to_string());
    assert_eq!(wire["thread"], "order-1");
    assert_eq!(wire["kind"], "bpm.message");
    assert_eq!(wire["text"], "ship it");
    assert_eq!(wire["sequence"], 1);
    assert!(
        wire.get("payload").is_none(),
        "the platform shape does not leak"
    );

    // Inbound: the peer's reply in its own shape, mapped back to the envelope.
    let mut world = World::new();
    world.peer.format = PeerFormat::Mapped(inbound_mapping());
    let reply = "{\"orderRef\":\"ACME-77\",\"thread\":\"order-1\",\"kind\":\"bpm.message\",\"version\":1,\"text\":\"shipped\"}";
    let response = world.receive(&from_peer(&world, reply, T0), T0).await;
    assert_eq!(response.status, 200, "{response:?}");
    let stored = world.inbox.stored().unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].1.message.payload.contains("shipped"));
    assert_eq!(stored[0].1.message.conversation.as_str(), "order-1");

    // A body that maps to nothing usable is refused after the signature, and stores nothing.
    let bad = "{\"unrelated\":true}";
    let response = world.receive(&from_peer(&world, bad, T0 + 5), T0 + 5).await;
    assert_eq!(
        (response.status, response.code),
        (400, Some(codes::INVALID_BODY))
    );
    assert_eq!(world.stored(), 1);

    // A mapping that fails to evaluate is a mapping failure, again after the signature.
    world.peer.format = PeerFormat::Mapped(MappingSpec {
        rules: vec![MappingRule {
            target: "id".to_owned(),
            source: String::new(),
            expression: "doc.orderRef +".to_owned(),
            coerce: Coercion::None,
        }],
    });
    let response = world
        .receive(&from_peer(&world, reply, T0 + 6), T0 + 6)
        .await;
    assert_eq!(
        (response.status, response.code),
        (400, Some(codes::MAPPING_FAILED))
    );
}

#[tokio::test]
async fn a_mapping_that_fails_outbound_holds_the_message() {
    let mut world = World::new();
    world.peer.format = PeerFormat::Mapped(MappingSpec {
        rules: vec![MappingRule {
            target: "x".to_owned(),
            source: String::new(),
            expression: "doc.id +".to_owned(),
            coerce: Coercion::None,
        }],
    });
    let id = world.enqueue("order-1", "held", T0);
    let peer = FakePeer::answering(200);
    let report = world.pump(&peer, T0 + 10).await;
    assert_eq!((report.attempted, report.retried), (1, 1));
    assert!(peer.bodies().is_empty());
    assert_eq!(world.state_of(&id), MessageState::Sent);
}

#[test]
fn a_record_topic_on_an_external_link_is_a_validation_error() {
    let mut record = grant(&topic(), Direction::Both);
    record.kind = spot_tenant_link::TopicKind::Record;
    let world = World::with(webhook_peer(), vec![grant(&topic(), Direction::Both)]);
    let mut link = world.link.clone();
    link.grants = vec![record];
    // The link model refuses it, and so does the peer check.
    assert!(link.validate().is_err());
    let error = world.peer.validate_for(&link).unwrap_err();
    assert_eq!(error.code(), codes::RECORD_TOPIC);
}

#[test]
fn a_peer_must_fit_its_link() {
    let world = World::new();
    assert!(world.peer.validate_for(&world.link).is_ok());
    // Not an external link.
    let mut direct = world.link.clone();
    direct.carrier = spot_tenant_link::Carrier::Direct;
    assert_eq!(
        world.peer.validate_for(&direct).unwrap_err().code(),
        codes::WRONG_CARRIER
    );
    // Another peer's link.
    let mut stranger = world.peer.clone();
    stranger.id = spot_tenant_link::ExternalPeerId::new("someone-else").unwrap();
    assert_eq!(
        stranger.validate_for(&world.link).unwrap_err().code(),
        codes::PEER_MISMATCH
    );
    // Malformed definitions.
    let mut bad = world.peer.clone();
    bad.inbound.path = "no-slash".to_owned();
    assert_eq!(bad.validate().unwrap_err().code(), codes::INVALID_PEER);
    let mut bad = world.peer.clone();
    bad.outbound = Outbound::WebhookUrl("ftp://acme.example".to_owned());
    assert_eq!(bad.validate().unwrap_err().code(), codes::INVALID_PEER);
    let mut bad = world.peer.clone();
    bad.inbound.tolerance_secs = 0;
    assert!(bad.validate().is_err());
    // The tolerance is bounded: 900 seconds is the widest accepted, 901 is rejected.
    let mut bad = world.peer.clone();
    bad.inbound.tolerance_secs = 901;
    assert_eq!(
        bad.validate().unwrap_err().code(),
        codes::TOLERANCE_TOO_WIDE
    );
    let mut widest = world.peer.clone();
    widest.inbound.tolerance_secs = spot_link_external::ExternalPeer::MAX_TOLERANCE_SECS;
    assert_eq!(widest.inbound.tolerance_secs, 900);
    assert!(widest.validate().is_ok());
    // The limits are required fields, and the constructor helper documents their defaults.
    let mut bad = world.peer.clone();
    bad.max_body_bytes = 0;
    assert_eq!(bad.validate().unwrap_err().code(), codes::INVALID_PEER);
    let mut bad = world.peer.clone();
    bad.refusal_window_secs = 0;
    assert_eq!(bad.validate().unwrap_err().code(), codes::INVALID_PEER);
    assert_eq!(
        world.peer.max_body_bytes,
        spot_link_external::ExternalPeer::DEFAULT_MAX_BODY_BYTES
    );
    assert_eq!(world.peer.max_body_bytes, 1024 * 1024);
    assert_eq!(
        world.peer.refusal_window_secs,
        spot_link_external::ExternalPeer::DEFAULT_REFUSAL_WINDOW_SECS
    );
    assert_eq!(world.peer.refusal_window_secs, 60);
    let mut bad = world.peer.clone();
    bad.name = " ".to_owned();
    assert!(bad.validate().is_err());
}
