//! The two real channels (signed webhook through egress, connector) against fakes, and the
//! canary test: the shared secret never leaves the vault.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use http::{HeaderMap, StatusCode};
use spot_connectors::{ConnectorError, ConnectorId, ConnectorOutput};
use spot_egress::{EgressError, Response};
use spot_link_exchange::{MessageState, RejectCode};
use spot_link_external::{
    ConnectorChannel, ExternalPeer, Outbound, PeerFormat, SignatureScheme, WebhookChannel, codes,
    verify_inbound,
};
use spot_tenant_link::Direction;
use spot_webhooks::Egress;
use url::Url;

type Posted = (String, String, HeaderMap, Vec<u8>);

/// A fake webhook endpoint behind the egress policy.
struct FakeEgress {
    seen: Mutex<Vec<Posted>>,
    reply: Mutex<Result<(u16, String), EgressError>>,
}

impl FakeEgress {
    fn new(reply: Result<(u16, String), EgressError>) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            reply: Mutex::new(reply),
        })
    }
}

#[async_trait]
impl Egress for FakeEgress {
    async fn post(
        &self,
        tenant: &str,
        url: &Url,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<Response, EgressError> {
        self.seen
            .lock()
            .unwrap()
            .push((tenant.to_owned(), url.to_string(), headers, body));
        match &*self.reply.lock().unwrap() {
            Ok((status, text)) => Ok(Response {
                status: StatusCode::from_u16(*status).unwrap(),
                headers: HeaderMap::new(),
                body: text.clone().into_bytes(),
            }),
            Err(error) => Err(error.clone()),
        }
    }
}

#[tokio::test]
async fn the_webhook_channel_posts_a_body_the_peer_can_verify_through_egress() {
    let world = World::new();
    let id = world.enqueue("order-1", "signed", T0);
    let egress = FakeEgress::new(Ok((200, "{\"status\":\"applied\"}".to_owned())));
    let channel = WebhookChannel::new(egress.clone(), world.vault.clone(), "tenant-a");
    let report = world.pump(&channel, T0 + 2_000).await;
    assert_eq!(report.applied, 1);
    assert_eq!(world.state_of(&id), MessageState::Applied);

    let seen = egress.seen.lock().unwrap();
    let (tenant, url, headers, body) = &seen[0];
    assert_eq!(tenant, "tenant-a");
    assert_eq!(url, "https://acme.example/hooks/spot");
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["x-spot-event-id"].to_str().unwrap(), id.to_string());
    // The peer, holding the same secret, verifies exactly what was posted.
    let pairs = vec![(
        "x-spot-signature".to_owned(),
        headers["x-spot-signature"].to_str().unwrap().to_owned(),
    )];
    verify_inbound(
        &SignatureScheme::SpotHmacSha256,
        SECRET.as_bytes(),
        &pairs,
        body,
        T0 + 2_000,
        300,
    )
    .unwrap();
    assert!(
        verify_inbound(
            &SignatureScheme::SpotHmacSha256,
            b"not the secret",
            &pairs,
            body,
            T0 + 2_000,
            300
        )
        .is_err()
    );
    // The tolerance is bounded even for a direct caller that skipped peer validation: a
    // window wider than 900 seconds would accept a replay long after it was sent.
    assert_eq!(
        verify_inbound(
            &SignatureScheme::SpotHmacSha256,
            SECRET.as_bytes(),
            &pairs,
            body,
            T0 + 2_000,
            901
        )
        .unwrap_err()
        .code(),
        codes::TOLERANCE_TOO_WIDE
    );
    assert!(
        verify_inbound(
            &SignatureScheme::SpotHmacSha256,
            SECRET.as_bytes(),
            &pairs,
            body,
            T0 + 2_000,
            0
        )
        .is_err()
    );
}

#[tokio::test]
async fn egress_refusals_and_missing_credentials_hold_the_message_and_are_audited() {
    let world = World::new();
    let id = world.enqueue("order-1", "held", T0);
    let denied = FakeEgress::new(Err(EgressError::PolicyDenied));
    let channel = WebhookChannel::new(denied.clone(), world.vault.clone(), "tenant-a");
    let report = world.pump(&channel, T0 + 2_000).await;
    assert_eq!((report.attempted, report.retried), (1, 1));
    assert_eq!(world.state_of(&id), MessageState::Sent);
    assert!(format!("{:?}", world.audit.entries().unwrap()).contains(codes::EGRESS_DENIED));

    // The peer's credential is not in the vault: nothing is posted at all.
    let mut world = World::new();
    world.peer.auth = spot_connectors::CredentialRef::new("gone").unwrap();
    let id = world.enqueue("order-1", "held", T0);
    let egress = FakeEgress::new(Ok((200, String::new())));
    let channel = WebhookChannel::new(egress.clone(), world.vault.clone(), "tenant-a");
    assert_eq!(world.pump(&channel, T0 + 2_000).await.retried, 1);
    assert!(egress.seen.lock().unwrap().is_empty());
    assert_eq!(world.state_of(&id), MessageState::Sent);

    // A connector peer pointed at a webhook channel is a configuration fault, not a rejection.
    let world = World::with(
        peer(
            Outbound::ConnectorOp {
                connector: ConnectorId::new("acme").unwrap(),
                operation: "post".to_owned(),
            },
            PeerFormat::JsonEnvelope,
        ),
        vec![grant(&topic(), Direction::Both)],
    );
    let id = world.enqueue("order-1", "held", T0);
    let channel = WebhookChannel::new(
        FakeEgress::new(Ok((200, String::new()))),
        world.vault.clone(),
        "t",
    );
    assert_eq!(world.pump(&channel, T0 + 2_000).await.retried, 1);
    assert_eq!(world.state_of(&id), MessageState::Sent);
}

fn connector_world() -> World {
    World::with(
        peer(
            Outbound::ConnectorOp {
                connector: ConnectorId::new("acme-erp").unwrap(),
                operation: "post_message".to_owned(),
            },
            PeerFormat::JsonEnvelope,
        ),
        vec![grant(&topic(), Direction::Both)],
    )
}

#[tokio::test]
async fn the_connector_channel_maps_results_to_delivery_states() {
    let world = connector_world();
    let accepted = world.enqueue("order-1", "one", T0);
    let refused = world.enqueue("order-2", "two", T0);
    let down = world.enqueue("order-3", "three", T0);
    let broken = world.enqueue("order-4", "four", T0);
    let port = Arc::new(FakeConnector::new(vec![
        Ok(ConnectorOutput {
            value: serde_json::json!({"status": "applied"}),
        }),
        Err(ConnectorError::Guest {
            code: "invalid".to_owned(),
            message: "no".to_owned(),
            retryable: false,
        }),
        Err(ConnectorError::Timeout { millis: 30_000 }),
        Err(ConnectorError::NotFound {
            id: "acme-erp".to_owned(),
        }),
    ]));
    let channel = ConnectorChannel::new(port.clone(), "tenant-a", 30_000);
    let report = world.pump(&channel, T0 + 2_000).await;
    assert_eq!((report.applied, report.rejected, report.retried), (1, 1, 2));
    assert_eq!(world.state_of(&accepted), MessageState::Applied);
    assert_eq!(
        world.state_of(&refused),
        MessageState::Rejected(RejectCode::PayloadInvalid)
    );
    assert_eq!(world.state_of(&down), MessageState::Sent);
    assert_eq!(world.state_of(&broken), MessageState::Sent);

    let calls = port.calls.lock().unwrap();
    assert_eq!(calls[0].operation, "post_message");
    assert_eq!(calls[0].connector.as_str(), "acme-erp");
    assert_eq!(calls[0].tenant, "tenant-a");
    assert_eq!(calls[0].input["payload"]["message.body"], "one");
    assert!(calls[0].deadline_micros > calls[0].now_micros);
}

/// Runs every path that touches the secret and returns everything a log, an audit record, a
/// receipt, a response or a screen could show.
async fn everything_after_every_path() -> (String, String) {
    let world = World::new();
    let egress = FakeEgress::new(Ok((200, "{\"status\":\"applied\"}".to_owned())));
    let channel = WebhookChannel::new(egress.clone(), world.vault.clone(), "tenant-a");
    let mut shown = String::new();
    world.enqueue("order-1", "out one", T0);
    shown += &format!("{:?}", world.pump(&channel, T0 + 2_000).await);
    *egress.reply.lock().unwrap() = Err(EgressError::PolicyDenied);
    world.enqueue("order-1", "out two", T0 + 3_000);
    shown += &format!("{:?}", world.pump(&channel, T0 + 5_000).await);
    *egress.reply.lock().unwrap() = Ok((422, "echo CANARY".to_owned()));
    shown += &format!("{:?}", world.pump(&channel, T0 + 10_000).await);

    let at = i64::try_from(T0 / 1_000).unwrap();
    let path = world.peer.inbound.path.clone();
    let good = from_peer(&world, &message_body("A", "c", "in one"), T0);
    let bad = signed(&message_body("B", "c", "in two"), &path, "wrong-secret", at);
    let junk = from_peer(&world, "{{{", T0);
    for request in [good, bad.clone(), junk] {
        shown += &format!("{:?}|{:?}", request, world.receive(&request, T0).await);
    }
    shown += &format!("{:?}", world.peer);
    shown += &format!("{:?}", world.bridge);
    shown += &format!("{:?}", world.link);
    shown += &world.everything();
    // What the egress saw (headers and bodies) is also output of this crate.
    let outgoing = format!("{:?}", egress.seen.lock().unwrap());
    (shown, outgoing)
}

#[tokio::test]
async fn the_shared_secret_never_appears_in_anything_the_crate_shows() {
    let (shown, outgoing) = everything_after_every_path().await;
    for hay in [&shown, &outgoing] {
        assert!(!hay.contains(SECRET), "the secret leaked");
        assert!(!hay.contains("CANARY-shared"), "part of the secret leaked");
        assert!(!hay.contains("wrong-secret"));
    }
    // The canary test is only worth something if the paths ran: they left their marks.
    assert!(shown.contains("link_external.bad_signature"));
    assert!(shown.contains(codes::EGRESS_DENIED));
    // Business data is not in the audit trail or the debug output either.
    assert!(!shown.contains("in one") && !shown.contains("out one"));
}

#[test]
fn the_peer_names_its_credential_by_reference_only() {
    let peer: ExternalPeer = webhook_peer();
    let shown = format!("{peer:?}");
    assert!(shown.contains("acme-shared-secret"));
    assert!(!shown.contains(SECRET));
}
