//! The inbound carrier: signature first, then the tenant link's own receive path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use common::*;
use spot_audit_chain::PayloadValue;
use spot_link_external::{InboundRequest, SignatureScheme, codes};
use spot_tenant_link::Direction;

fn audit_text(world: &World) -> String {
    format!("{:?}", world.audit.entries().unwrap())
}

/// Every `tenant_link.refused` row as `(detail, count)`: a detail row names the code of the
/// first refusal of a window; a summary row carries the window's total under `count`.
fn refusal_rows(world: &World) -> Vec<(Option<String>, Option<i64>)> {
    world
        .audit
        .entries()
        .unwrap()
        .into_iter()
        .filter(|record| record.kind == "tenant_link.refused")
        .map(|record| {
            let detail = match record.payload.get("detail") {
                Some(PayloadValue::Text(text)) => Some(text.clone()),
                _ => None,
            };
            let count = match record.payload.get("count") {
                Some(PayloadValue::Int(count)) => Some(*count),
                _ => None,
            };
            (detail, count)
        })
        .collect()
}

#[tokio::test]
async fn a_signed_message_is_stored_in_the_inbox() {
    let world = World::new();
    let body = message_body("PO-1001", "order-1", "hello from acme");
    let response = world.receive(&from_peer(&world, &body, T0), T0).await;
    assert_eq!(
        (response.status, response.code),
        (200, None),
        "{response:?}"
    );
    assert_eq!(world.stored(), 1);
    let stored = world.inbox.stored().unwrap();
    assert_eq!(stored[0].1.message.topic, topic());
    assert_eq!(stored[0].1.message.seq, 1);
    // The second message of the conversation is numbered next, in arrival order.
    let next = message_body("PO-1002", "order-1", "second");
    assert_eq!(
        world
            .receive(&from_peer(&world, &next, T0 + 1_000), T0 + 1_000)
            .await
            .status,
        200
    );
    assert_eq!(world.stored(), 2);
    assert!(
        world
            .inbox
            .stored()
            .unwrap()
            .iter()
            .any(|(_, e)| e.message.seq == 2)
    );
}

#[tokio::test]
async fn a_replay_of_the_same_peer_id_is_recognised_and_stored_once() {
    let world = World::new();
    let body = message_body("PO-1001", "order-1", "once");
    let first = world.receive(&from_peer(&world, &body, T0), T0).await;
    let again = world
        .receive(&from_peer(&world, &body, T0 + 5_000), T0 + 5_000)
        .await;
    assert_eq!(first.body, "{\"status\":\"accepted\"}");
    assert_eq!(again.status, 200);
    assert_eq!(again.body, "{\"status\":\"duplicate\"}");
    assert_eq!(world.stored(), 1);
    // The same body replayed with its original signature, later, inside the window.
    let replay = from_peer(&world, &body, T0);
    assert_eq!(
        world.receive(&replay, T0 + 60_000).await.body,
        "{\"status\":\"duplicate\"}"
    );
    assert_eq!(world.stored(), 1);
}

#[tokio::test]
async fn nothing_is_accepted_without_a_valid_signature() {
    let world = World::new();
    let body = message_body("PO-1", "order-1", "x");
    let at = i64::try_from(T0 / 1_000).unwrap();
    let path = world.peer.inbound.path.clone();

    // A wrong secret, a tampered body, no header, a garbled header, a stale and a future stamp.
    let wrong_secret = signed(&body, &path, "some-other-secret", at);
    // Headers signed over one body, sent with another: the signature does not cover it.
    let tampered = signed_over(
        &body,
        &message_body("PO-1", "order-1", "y"),
        &path,
        SECRET,
        at,
    );
    let mut unsigned = signed(&body, &path, SECRET, at);
    unsigned
        .headers
        .retain(|(name, _)| name != "X-Spot-Signature");
    let mut garbled = signed(&body, &path, SECRET, at);
    garbled.headers[1].1 = "v1=abc".to_owned();
    let stale = signed(&body, &path, SECRET, at - 301);
    let future = signed(&body, &path, SECRET, at + 301);

    for (name, request, code) in [
        ("wrong secret", wrong_secret, codes::BAD_SIGNATURE),
        ("tampered", tampered, codes::BAD_SIGNATURE),
        ("unsigned", unsigned, codes::MISSING_SIGNATURE),
        ("garbled", garbled, codes::MISSING_SIGNATURE),
        ("stale", stale, codes::STALE_TIMESTAMP),
        ("future", future, codes::STALE_TIMESTAMP),
    ] {
        let response = world.receive(&request, T0).await;
        assert_eq!(
            (response.status, response.code),
            (401, Some(code)),
            "{name}"
        );
    }
    assert_eq!(world.stored(), 0);
    assert!(world.quarantine.held().is_empty());
    assert!(audit_text(&world).contains(codes::BAD_SIGNATURE));
}

#[tokio::test]
async fn the_signature_is_checked_before_the_body_is_read() {
    let world = World::new();
    // Not JSON at all, badly signed: the answer is about the signature, not the body.
    let junk = signed("{{{ not json", &world.peer.inbound.path, "wrong", 1);
    let response = world.receive(&junk, T0).await;
    assert_eq!(response.code, Some(codes::STALE_TIMESTAMP));
    let junk = signed(
        "{{{ not json",
        &world.peer.inbound.path,
        "wrong",
        i64::try_from(T0 / 1_000).unwrap(),
    );
    assert_eq!(
        world.receive(&junk, T0).await.code,
        Some(codes::BAD_SIGNATURE)
    );
    // Well signed but not a message: now the body is the problem.
    let junk = from_peer(&world, "{{{ not json", T0);
    assert_eq!(
        world.receive(&junk, T0).await.code,
        Some(codes::INVALID_BODY)
    );
    // And a path that is not the peer's is not answered at all.
    let elsewhere = signed(
        "{}",
        "/links/other/inbound",
        SECRET,
        i64::try_from(T0 / 1_000).unwrap(),
    );
    assert_eq!(world.receive(&elsewhere, T0).await.status, 404);
}

#[tokio::test]
async fn a_header_scheme_peer_is_verified_the_same_way() {
    let mut peer = webhook_peer();
    peer.inbound.signature_scheme = SignatureScheme::HeaderHmacSha256 {
        signature_header: "x-acme-signature".to_owned(),
        timestamp_header: "x-acme-timestamp".to_owned(),
    };
    let world = World::with(peer, vec![grant(&topic(), Direction::Both)]);
    let body = message_body("PO-1", "order-1", "hi");
    let at = i64::try_from(T0 / 1_000).unwrap();
    let signature = spot_webhooks::SigningKey::new(SECRET.as_bytes()).sign(at, body.as_bytes());
    let mut request = signed(&body, &world.peer.inbound.path, SECRET, at);
    request.headers = vec![
        ("X-Acme-Signature".to_owned(), signature),
        ("X-Acme-Timestamp".to_owned(), at.to_string()),
    ];
    assert_eq!(world.receive(&request, T0).await.status, 200);
    request.headers[0].1 = "00".repeat(32);
    assert_eq!(
        world.receive(&request, T0).await.code,
        Some(codes::BAD_SIGNATURE)
    );
}

#[tokio::test]
async fn the_peer_cannot_send_beyond_its_grant() {
    let world = World::new();
    let send = |body: String, now: u64| {
        let request = from_peer(&world, &body, now);
        let world = &world;
        async move { world.receive(&request, now).await }
    };
    // A topic that is not granted.
    let other = message_body("A", "c", "x").replace("bpm.message", "bpm.other");
    let response = send(other, T0).await;
    assert_eq!(
        (response.status, response.code),
        (403, Some("link_exchange.not_granted"))
    );
    // A schema version outside the granted range.
    let future =
        message_body("B", "c", "x").replace("\"schema_version\":1", "\"schema_version\":9");
    let response = send(future, T0 + 1).await;
    assert_eq!(
        (response.status, response.code),
        (426, Some("link_exchange.version_unsupported"))
    );
    // A field the grant's field list does not name.
    let secret_field = message_body("C", "c", "x").replace("message.body", "message.secret_notes");
    let response = send(secret_field, T0 + 2).await;
    assert_eq!(
        (response.status, response.code),
        (422, Some("link_exchange.payload_invalid"))
    );
    // A float never crosses.
    let float = message_body("D", "c", "x").replace("\"x\"", "1.5");
    assert_eq!(send(float, T0 + 3).await.code, Some(codes::INVALID_BODY));
    assert_eq!(world.stored(), 0);

    // A grant that only lets us send does not let the peer send.
    let world = World::with(webhook_peer(), vec![grant(&topic(), Direction::Send)]);
    let body = message_body("E", "c", "x");
    let response = world.receive(&from_peer(&world, &body, T0), T0).await;
    assert_eq!(
        (response.status, response.code),
        (403, Some("link_exchange.not_granted"))
    );
}

#[tokio::test]
async fn revocation_stops_both_directions() {
    let mut world = World::new();
    let queued = world.enqueue("order-1", "out", T0);
    let body = message_body("PO-1", "order-1", "in");
    assert_eq!(
        world
            .receive(&from_peer(&world, &body, T0), T0)
            .await
            .status,
        200
    );
    world.revoke(T0 + 10);

    let again = message_body("PO-2", "order-1", "after revoke");
    let response = world
        .receive(&from_peer(&world, &again, T0 + 20), T0 + 20)
        .await;
    assert_eq!(
        (response.status, response.code),
        (403, Some(codes::LINK_INACTIVE))
    );
    assert_eq!(world.stored(), 1);

    let peer = FakePeer::answering(200);
    assert_eq!(world.pump(&peer, T0 + 30).await.attempted, 0);
    assert!(peer.bodies().is_empty());
    assert_eq!(
        world.state_of(&queued),
        spot_link_exchange::MessageState::Queued
    );
}

#[tokio::test]
async fn an_unresolvable_credential_refuses_everything_and_is_retryable() {
    let mut world = World::new();
    world.peer.auth = spot_connectors::CredentialRef::new("not-in-the-vault").unwrap();
    let body = message_body("PO-1", "order-1", "x");
    let response = world.receive(&from_peer(&world, &body, T0), T0).await;
    assert_eq!(
        (response.status, response.code),
        (503, Some(codes::CREDENTIAL_UNAVAILABLE))
    );
    assert_eq!(world.stored(), 0);
}

#[tokio::test]
async fn an_oversized_body_is_refused_before_the_signature_and_the_secret() {
    let mut world = World::new();
    world.peer.max_body_bytes = 64;
    let body = message_body("PO-1", "order-1", &"x".repeat(200));
    let request = from_peer(&world, &body, T0);
    assert!(u64::try_from(request.body().len()).unwrap() > world.peer.max_body_bytes);
    let vault = SpyVault::new();
    let response = world.receive_with(&vault, &request, T0).await;
    assert_eq!(
        (response.status, response.code),
        (413, Some(codes::BODY_TOO_LARGE)),
        "{response:?}"
    );
    assert!(
        !vault.resolved(),
        "an oversized body must be refused before the signing key is resolved"
    );
    // The answer is fixed: it names neither the configured cap nor anything the peer sent.
    assert_eq!(
        response.body,
        format!("{{\"error\":{{\"code\":\"{}\"}}}}", codes::BODY_TOO_LARGE)
    );
    assert_eq!(world.stored(), 0);
    // It was counted, not written per request.
    assert_eq!(refusal_rows(&world).len(), 1);
}

#[tokio::test]
async fn the_cap_holds_for_a_request_with_no_content_length_and_no_signature() {
    // Nothing but a path and bytes: an edge that forwards a chunked body with no
    // `Content-Length` and no signature still gets the size answer, because the count is on
    // the bytes received, never on a header.
    let mut world = World::new();
    world.peer.max_body_bytes = 8;
    let path = world.peer.inbound.path.clone();
    let error = world.read(&path, Vec::new(), Bytes::new(9, 1)).unwrap_err();
    assert_eq!(error.code(), codes::BODY_TOO_LARGE);
    // A body exactly at the cap is not refused for its size (it is a wire problem, not a
    // size problem).
    let at_cap = InboundRequest::from_stream(
        path,
        Vec::new(),
        [Ok::<_, std::io::Error>(vec![b'x'; 8])],
        8,
    )
    .expect("a body at the cap is accepted");
    assert_eq!(
        world.receive(&at_cap, T0).await.code,
        Some(codes::MISSING_SIGNATURE)
    );
}

#[tokio::test]
async fn a_flood_of_bad_signatures_writes_one_detail_row_and_one_summary() {
    let world = World::new();
    let body = message_body("PO-1", "order-1", "x");
    let at = i64::try_from(T0 / 1_000).unwrap();
    let bad = signed(&body, &world.peer.inbound.path, "wrong-secret", at);

    for _ in 0..1_000 {
        assert_eq!(world.receive(&bad, T0).await.status, 401);
    }
    // One detail row, not a thousand.
    let rows = refusal_rows(&world);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0.as_deref(), Some(codes::BAD_SIGNATURE));
    assert_eq!(rows[0].1, None);

    // The first request of the next window closes the window: one summary row with the rest.
    let good = message_body("PO-2", "order-1", "hello");
    assert_eq!(
        world
            .receive(&from_peer(&world, &good, T0 + 60_000), T0 + 60_000)
            .await
            .status,
        200
    );
    let rows = refusal_rows(&world);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].0, None);
    assert_eq!(rows[1].1, Some(999));

    // The next window starts fresh: its first refusal is a detail row again.
    let later = T0 + 120_000;
    let bad_again = signed(
        &body,
        &world.peer.inbound.path,
        "wrong-secret",
        i64::try_from(later / 1_000).unwrap(),
    );
    assert_eq!(world.receive(&bad_again, later).await.status, 401);
    let rows = refusal_rows(&world);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[2].0.as_deref(), Some(codes::BAD_SIGNATURE));
    assert_eq!(rows[2].1, None);
}
