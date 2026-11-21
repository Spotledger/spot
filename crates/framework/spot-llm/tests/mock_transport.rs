//! The mock transport: recorded exchanges replay exactly, headers stay redacted, unmatched
//! requests fail with a typed error.

use spot_llm::transport::mock::{MockTransport, RecordedExchange, RecordedResponse};
use spot_llm::transport::{HttpMethod, HttpRequest, HttpResponse, HttpTransport};
/// A recorded exchange: one POST with a secret-shaped header, answered with `response`.
fn recording(url: &str, response: RecordedResponse) -> RecordedExchange {
    RecordedExchange {
        request: HttpRequest::post_json(url, "{}")
            .with_header("x-api-key", "sk-replayed-exchange")
            .with_header("authorization", "Bearer sk-replayed-exchange"),
        response,
    }
}

/// Sends `request`, panicking when the transport fails.
async fn sent(transport: &dyn HttpTransport, request: HttpRequest) -> HttpResponse {
    match transport.send(request).await {
        Ok(response) => response,
        Err(e) => panic!("send failed: {e}"),
    }
}

#[tokio::test]
async fn replays_a_complete_response() {
    let transport = MockTransport::new(vec![recording(
        "https://api.test/v1/messages",
        RecordedResponse::Complete {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: "{\"ok\":true}".to_owned(),
        },
    )]);
    let response = sent(
        &transport,
        HttpRequest::post_json("https://api.test/v1/messages", "{\"m\":1}"),
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "{\"ok\":true}");
    assert_eq!(response.headers[0].1, "application/json");
}

#[tokio::test]
async fn replays_sse_chunks_and_buffers_streams_on_send() {
    let transport = MockTransport::new(vec![recording(
        "https://api.test/v1/messages",
        RecordedResponse::Stream {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            chunks: vec!["data: one\n\n".to_owned(), "data: two\n\n".to_owned()],
        },
    )]);
    let mut stream = match transport
        .send_stream(HttpRequest::post_json("https://api.test/v1/messages", "{}"))
        .await
    {
        Ok(stream) => stream,
        Err(e) => panic!("send_stream failed: {e}"),
    };
    let mut got = String::new();
    while let Some(chunk) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        match chunk {
            Ok(bytes) => got.push_str(&String::from_utf8(bytes).unwrap_or_default()),
            Err(e) => panic!("stream yielded {e}"),
        }
    }
    assert_eq!(got, "data: one\n\ndata: two\n\n");

    // A buffered read over the same recording concatenates the chunks.
    let response = sent(
        &transport,
        HttpRequest::post_json("https://api.test/v1/messages", "{}"),
    )
    .await;
    assert_eq!(response.body, "data: one\n\ndata: two\n\n");
}

#[tokio::test]
async fn unmatched_requests_fail_typed() {
    let transport = MockTransport::new(vec![recording(
        "https://api.test/v1/messages",
        RecordedResponse::Complete {
            status: 200,
            headers: vec![],
            body: String::new(),
        },
    )]);
    let error = match transport
        .send(HttpRequest::post_json("https://elsewhere.test/v1", "{}"))
        .await
    {
        Ok(_) => panic!("an unmatched request must fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "transport.protocol");
    assert!(!error.retryable());
    assert!(error.to_string().contains("https://elsewhere.test/v1"));
}

#[test]
fn debug_redacts_secret_headers_and_bodies() {
    let request =
        HttpRequest::post_json("https://api.test/v1/messages", "{\"seed\":\"top-secret\"}")
            .with_header("x-api-key", "sk-replayed-exchange")
            .with_header("x-safe", "plain");
    let debug = format!("{request:?}");
    assert!(
        !debug.contains("sk-replayed-exchange"),
        "auth headers are redacted: {debug}"
    );
    assert!(!debug.contains("top-secret"), "bodies never print: {debug}");
    assert!(debug.contains("body_len"), "only the body length prints");
    assert!(debug.contains("plain"), "harmless headers stay readable");
}

#[tokio::test]
async fn recordings_round_trip_through_files() {
    let exchanges = vec![recording(
        "https://api.test/v1/messages",
        RecordedResponse::Complete {
            status: 429,
            headers: vec![],
            body: "rate limited".to_owned(),
        },
    )];
    let transport = MockTransport::new(exchanges.clone());

    let path = format!(
        "{}/.spot-llm-mock-{}.json",
        std::env::temp_dir().display(),
        std::process::id()
    );
    let file_body = serde_json::to_string(&exchanges).ok().unwrap_or_default();
    let wrote = std::fs::write(&path, file_body).is_ok();
    assert!(wrote, "the recording file must be writable");

    let loaded = MockTransport::load_file(&path);
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            panic!("load_file failed: {e}");
        }
    };
    assert_eq!(loaded.exchanges(), transport.exchanges());
    let response = sent(
        &loaded,
        HttpRequest::post_json("https://api.test/v1/messages", "{}"),
    )
    .await;
    assert_eq!(response.status, 429);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn the_transport_is_object_safe() {
    let boxed: std::sync::Arc<dyn HttpTransport> =
        std::sync::Arc::new(MockTransport::new(vec![recording(
            "https://api.test/v1/messages",
            RecordedResponse::Complete {
                status: 200,
                headers: vec![],
                body: "boxed".to_owned(),
            },
        )]));
    let response = sent(
        boxed.as_ref(),
        HttpRequest::post_json("https://api.test/v1/messages", "{}"),
    )
    .await;
    assert_eq!(response.body, "boxed");
}

#[test]
fn method_wire_names() {
    let wire = serde_json::to_value(HttpMethod::Post)
        .ok()
        .unwrap_or_default();
    assert_eq!(wire, serde_json::json!("POST"));
    assert_eq!(HttpMethod::Get.as_str(), "GET");
}
