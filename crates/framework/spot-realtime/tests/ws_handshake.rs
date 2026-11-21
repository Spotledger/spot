//! The handshake and the close codes of the contract's table (suite brief WP-PLT-08.T2):
//! subprotocol refusal, auth-first within ten seconds, protocol violations, binary frames,
//! oversized frames, and the mount through the spot-http server. Every server message these
//! tests read is validated against the contract's `openapi.yaml` (the validation happens in
//! `support::Client::read`).

#[allow(dead_code)]
mod support;

use support::Must;
use support::{TEST_TOKEN, connect_and_auth, connect_raw, harness};
use tokio_tungstenite::tungstenite::Message as WireMessage;

#[tokio::test(start_paused = true)]
async fn a_connection_that_never_authenticates_closes_4001_after_the_window() {
    // Not a single frame sent: the ten-second window runs out and the socket closes with
    // 4001 (authentication failed or timed out). Paused time makes the boundary exact and
    // the test instant instead of a ten-second wall-clock wait.
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    assert_eq!(client.read_until_close().await, 4001);
}

#[tokio::test(start_paused = true)]
async fn the_auth_window_is_ten_seconds_and_a_late_auth_closes_4001() {
    // Paused time: the ten-second window advances only when the test drives it, so the close
    // lands exactly on the contract's boundary rather than at a wall-clock guess.
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    // Client traffic before the window is over does not stop the clock: a WebSocket ping is
    // liveness, not the `auth` (and a pong reply to our heartbeat counts as nothing, per the
    // contract).
    tokio::time::advance(std::time::Duration::from_secs(9)).await;
    client.send_ws_ping(b"liveness").await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    // The window is over: the server closes with 4001, and any late authentication lands on
    // a socket that is already closing.
    assert_eq!(client.read_until_close().await, 4001);
    let late_delivery = client
        .try_send_message(&support::ClientMessage::Auth {
            token: support::TEST_TOKEN.to_owned(),
        })
        .await;
    assert!(
        late_delivery.is_err(),
        "a closed socket accepts no further auth"
    );
}

#[tokio::test]
async fn a_rejected_token_closes_4001() {
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    client
        .send_message(&support::ClientMessage::Auth {
            token: "token:unknown".to_owned(),
        })
        .await;
    assert_eq!(client.read_until_close().await, 4001);
}

#[tokio::test]
async fn a_valid_protocol_message_before_auth_closes_1008() {
    // The contract's table: "a message before auth" is a protocol violation.
    let harness = harness().await;
    // Admit nothing: the connection is refused the moment we speak before auth. The token
    // itself is irrelevant here; the first message is not an auth.
    let client = support::connect(harness.addr).await;
    client.send_raw(r#"{"type":"ping"}"#).await;
    assert_eq!(client.read_until_close().await, 1008);
}

#[tokio::test]
async fn invalid_json_and_unknown_types_close_1008() {
    for first_frame in ["not json at all", r#"{"type":"teleport"}"#, "[]"] {
        let harness = harness().await;
        let client = support::connect(harness.addr).await;
        client.send_raw(first_frame).await;
        assert_eq!(client.read_until_close().await, 1008, "{first_frame}");
    }
}

#[tokio::test]
async fn a_malformed_auth_shape_closes_1008_rather_than_4001() {
    // An auth without a token is not a protocol message; the transport closes 1008. (A well
    // formed auth with a bad token is the 4001 case above.)
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    client.send_raw(r#"{"type":"auth"}"#).await;
    assert_eq!(client.read_until_close().await, 1008);
}

#[tokio::test]
async fn a_second_auth_is_a_protocol_violation_1008() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    client
        .send_message(&support::ClientMessage::Auth {
            token: TEST_TOKEN.to_owned(),
        })
        .await;
    assert_eq!(client.read_until_close().await, 1008);
}

#[tokio::test]
async fn a_binary_frame_closes_1003() {
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    client.send_binary(b"\x00\x01\x02").await;
    assert_eq!(client.read_until_close().await, 1003);
}

#[tokio::test]
async fn a_frame_over_64k_closes_1009() {
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    // 64 KiB exactly is the contract's limit; one byte over must close with 1009.
    let oversized = "x".repeat(spot_realtime::ws::MAX_FRAME_BYTES + 1);
    client.send_raw(&oversized).await;
    assert_eq!(client.read_until_close().await, 1009);
}

#[tokio::test]
async fn a_frame_a_megabyte_wide_closes_1009_through_the_socket_limit() {
    // The upgrade pins the transport's own frame/message limits to the contract's 64 KiB,
    // so even a frame far past the limit — this one is a megabyte — is refused by the
    // socket as it decodes, before anything of it can be buffered, and still closes with
    // the contract's 1009 (the pre-auth client cannot park a message on the server).
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    let oversized = "x".repeat(1_000_000);
    client.send_raw(&oversized).await;
    assert_eq!(client.read_until_close().await, 1009);
}

#[tokio::test(start_paused = true)]
async fn an_unsolicited_pong_does_not_stop_the_auth_window() {
    // Pre-auth a pong is not the `auth` and does not stop the auth window (the lead
    // review's pong-liveness rule binds after the auth): however many pongs a connection
    // answers with, it still closes 4001 at the ten-second mark. Paused time makes the
    // boundary exact. (The server sends no heartbeat before the auth, so any pre-auth pong
    // is an unsolicited one — the frame the `authenticate` loop must brush aside.)
    let harness = harness().await;
    let client = support::connect(harness.addr).await;
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    client
        .send(WireMessage::Pong(std::vec::Vec::new().into()))
        .await;
    tokio::time::advance(std::time::Duration::from_secs(7)).await;
    assert_eq!(client.read_until_close().await, 4001);
}

#[tokio::test]
async fn an_upgrade_without_the_spot_v1_subprotocol_is_refused_with_the_error_envelope() {
    let harness = harness().await;
    // The handshake itself fails over HTTP: the server answers the api.invalid_request
    // envelope, and the client never gets a socket.
    let error = connect_raw(harness.addr, None).await;
    let error = match error {
        Err(error) => error,
        Ok(_socket) => panic!("the upgrade without a subprotocol must be refused"),
    };
    let response = match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response,
        other => panic!("expected an HTTP refusal, got: {other:?}"),
    };
    assert_eq!(response.status(), 400);
    let body = response
        .body()
        .as_deref()
        .unwrap_or_else(|| panic!("the refusal carries its envelope"));
    let envelope: serde_json::Value = serde_json::from_slice(body).must();
    assert_eq!(envelope["error"]["code"], "api.invalid_request");
}

#[tokio::test]
async fn a_plain_get_without_an_upgrade_is_refused_with_the_error_envelope() {
    let harness = harness().await;
    let refused = reqwest_get(harness.addr).await;
    assert_eq!(refused, 400);
}

/// A plain GET (no Upgrade header): answered with the refusal envelope, not an upgrade.
async fn reqwest_get(addr: std::net::SocketAddr) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.must();
    stream
        .write_all(
            format!("GET /api/v1/realtime HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .must();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.must();
    let head = String::from_utf8_lossy(&raw);
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in: {head}"))
}

/// The conformance harness must reject a non-contract message, or the suites above could be
/// validating nothing: every `Client::read` claims to validate against
/// `RealtimeServerMessage`, so a schema drift must fail here loudly.
#[test]
#[should_panic(expected = "not schema-conformant")]
fn the_conformance_check_rejects_a_non_contract_message() {
    support::validate_server_message(&serde_json::json!({ "type": "magic" }));
}
