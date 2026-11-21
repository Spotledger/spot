//! The transport framings: Streamable HTTP and stdio.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use spot_mcp::http::{HttpRequest, credential_of, handle, stdio_reply};
use spot_mcp::jsonrpc::Framing;
use spot_mcp::principal::Credential;
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

#[tokio::test]
async fn a_http_post_returns_json() {
    let (server, _meta) = support::server().await;
    let body = support::request(1, "tools/list", json!({}));
    let request = HttpRequest::new("POST", "/mcp", body.into_bytes())
        .with_header("authorization", "Bearer alice")
        .with_header("content-type", "application/json")
        .with_header("mcp-protocol-version", spot_mcp::PROTOCOL_VERSION);
    let response = handle(&server, &request, now()).await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert!(value["result"]["tools"].is_array());
}

#[tokio::test]
async fn a_http_get_is_refused() {
    let (server, _meta) = support::server().await;
    let request = HttpRequest::new("GET", "/mcp", Vec::new());
    let response = handle(&server, &request, now()).await;
    assert_eq!(response.status, 405);
}

#[tokio::test]
async fn an_unsupported_protocol_header_is_refused() {
    let (server, _meta) = support::server().await;
    let body = support::request(1, "tools/list", json!({}));
    let request = HttpRequest::new("POST", "/mcp", body.into_bytes())
        .with_header("authorization", "Bearer alice")
        .with_header("mcp-protocol-version", "1900-01-01");
    let response = handle(&server, &request, now()).await;
    assert_eq!(response.status, 400);
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(value["data"]["code"], "mcp.unsupported_protocol_version");
}

#[test]
fn credentials_are_read_from_the_headers() {
    let bearer =
        HttpRequest::new("POST", "/mcp", Vec::new()).with_header("authorization", "Bearer token-1");
    assert_eq!(credential_of(&bearer), Some(Credential::bearer("token-1")));
    let key = HttpRequest::new("POST", "/mcp", Vec::new()).with_header("x-api-key", "key-1:secret");
    assert_eq!(
        credential_of(&key),
        Some(Credential::api_key("key-1", "secret"))
    );
    let none = HttpRequest::new("POST", "/mcp", Vec::new());
    assert_eq!(credential_of(&none), None);
}

#[tokio::test]
async fn stdio_frames_messages() {
    let (server, _meta) = support::server().await;
    let body = support::request(1, "ping", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let bytes = stdio_reply(&messages, Framing::LineDelimited).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.ends_with('\n'));
    assert_eq!(text.lines().count(), 1);

    let framed = stdio_reply(&messages, Framing::ContentLength).unwrap();
    assert!(framed.starts_with(b"Content-Length: "));
}
