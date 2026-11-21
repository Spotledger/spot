//! Embeddings over the OpenAI-compatible wire (WP-AI-03.T6), on recorded exchanges: the exact
//! request (path, headers, JSON), the reply parsing (out-of-order items, wrong count, empty
//! vector, non-finite values) and the error mapping (401, 429, 5xx), with no network.

use std::sync::{Arc, Mutex, PoisonError};

use spot_llm::provider::BoxFuture;
use spot_llm::transport::mock::{MockTransport, RecordedExchange, RecordedResponse};
use spot_llm::transport::{
    ByteStream, HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError,
};
use spot_llm::{ApiKey, EmbedRequest, EmbedResponse, Embedder as _, LlmError};
use spot_llm_openai::{OpenAiProvider, Profile};

/// The key every test plants; it must appear in no error and no `Debug` output.
const KEY: &str = "sk-planted-key-0123456789";

/// The input text every test plants; it must appear in no error.
const TEXT: &str = "planted-input-text-do-not-leak";

/// The base URL the recordings answer under (a trailing slash on purpose).
const BASE: &str = "https://embeddings.test/";

/// The URL a request lands on.
const URL: &str = "https://embeddings.test/v1/embeddings";

/// What a canned transport answers: status, headers and body, or a transport failure.
type Reply = Result<(u16, Vec<(String, String)>, String), TransportError>;

/// A transport that records every request it receives and answers with one canned reply.
struct Capture {
    reply: Reply,
    seen: Mutex<Vec<HttpRequest>>,
}

impl Capture {
    /// Answers every call with `status` and `body`.
    fn replying(status: u16, body: &str) -> Arc<Self> {
        Self::replying_with(status, Vec::new(), body)
    }

    /// Answers every call with `status`, `headers` and `body`.
    fn replying_with(status: u16, headers: Vec<(String, String)>, body: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: Ok((status, headers, body.to_owned())),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// Fails every call with `error`.
    fn failing(error: TransportError) -> Arc<Self> {
        Arc::new(Self {
            reply: Err(error),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// The requests received so far.
    fn requests(&self) -> Vec<HttpRequest> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl HttpTransport for Capture {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            match &self.reply {
                Ok((status, headers, body)) => Ok(HttpResponse {
                    status: *status,
                    headers: headers.clone(),
                    body: body.clone(),
                }),
                Err(error) => Err(error.clone()),
            }
        })
    }

    fn send_stream<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> BoxFuture<'a, Result<ByteStream, TransportError>> {
        Box::pin(async {
            Err(TransportError::Protocol(
                "the embeddings call never streams".to_owned(),
            ))
        })
    }
}

/// A provider over `transport` with `profile` and an optional key.
fn build(profile: Profile, key: Option<&str>, transport: Arc<Capture>) -> OpenAiProvider {
    OpenAiProvider::new(profile, BASE, key.map(ApiKey::new), transport, Vec::new())
}

/// The usual bearer provider.
fn openai(transport: Arc<Capture>) -> OpenAiProvider {
    build(Profile::generic(), Some(KEY), transport)
}

/// A request over `texts`.
fn request(texts: &[&str]) -> EmbedRequest {
    EmbedRequest::new(
        "text-embedding-3-small",
        texts.iter().map(|text| (*text).to_owned()).collect(),
    )
}

/// The reply body for `items` (`(index, embedding JSON)`), with usage.
fn reply_of(items: &[(u64, &str)]) -> String {
    let data: Vec<String> = items
        .iter()
        .map(|(index, embedding)| {
            format!(r#"{{"object":"embedding","index":{index},"embedding":{embedding}}}"#)
        })
        .collect();
    format!(
        r#"{{"object":"list","data":[{}],"model":"text-embedding-3-small","usage":{{"prompt_tokens":11,"total_tokens":11}}}}"#,
        data.join(",")
    )
}

/// Awaits a call that must succeed.
async fn ok(provider: &OpenAiProvider, request: EmbedRequest) -> EmbedResponse {
    match provider.embed(request).await {
        Ok(response) => response,
        Err(error) => panic!("the embedding must succeed: {error}"),
    }
}

/// Awaits a call that must fail.
async fn err(provider: &OpenAiProvider, request: EmbedRequest) -> LlmError {
    match provider.embed(request).await {
        Err(error) => error,
        Ok(response) => panic!("the embedding must fail, got {response:?}"),
    }
}

/// The request is exactly the documented JSON, at `{base}/v1/embeddings`, with the bearer
/// header; `dimensions` appears only when asked.
#[tokio::test]
async fn the_request_is_the_documented_json() {
    let transport = Capture::replying(200, &reply_of(&[(0, "[0.5,0.25]"), (1, "[1.0,2.0]")]));
    let provider = openai(transport.clone());

    ok(&provider, request(&["alpha", "beta"])).await;
    ok(&provider, request(&["alpha", "beta"]).with_dimensions(2)).await;

    let seen = transport.requests();
    assert_eq!(seen.len(), 2);
    let first = &seen[0];
    assert_eq!(first.method, HttpMethod::Post);
    assert_eq!(first.url, URL);
    assert_eq!(
        first.body,
        r#"{"model":"text-embedding-3-small","input":["alpha","beta"],"encoding_format":"float"}"#
    );
    assert!(
        first
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/json")
    );
    assert!(
        first
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == &format!("Bearer {KEY}"))
    );
    assert_eq!(
        seen[1].body,
        r#"{"model":"text-embedding-3-small","input":["alpha","beta"],"encoding_format":"float","dimensions":2}"#
    );
}

/// A keyless local profile sends no authorization header; extra headers are carried.
#[tokio::test]
async fn auth_style_and_extra_headers_follow_the_profile() {
    let transport = Capture::replying(200, &reply_of(&[(0, "[1.0]")]));
    let local = build(Profile::ollama(), Some(KEY), transport.clone());
    ok(&local, request(&["a"])).await;
    assert!(
        transport.requests()[0]
            .headers
            .iter()
            .all(|(name, _)| name != "authorization")
    );

    let transport = Capture::replying(200, &reply_of(&[(0, "[1.0]")]));
    let mut profile = Profile::openrouter();
    profile
        .extra_headers
        .push(("x-title".to_owned(), "spot".to_owned()));
    let routed = build(profile, Some(KEY), transport.clone());
    ok(&routed, request(&["a"])).await;
    assert!(
        transport.requests()[0]
            .headers
            .iter()
            .any(|(name, value)| name == "x-title" && value == "spot")
    );
}

/// Items arrive out of order; the vectors come back in input order with the reported usage.
#[tokio::test]
async fn out_of_order_items_are_sorted_by_index() {
    let body = reply_of(&[(2, "[3.0,3.5]"), (0, "[1.0,1.5]"), (1, "[2.0,2.5]")]);
    let provider = openai(Capture::replying(200, &body));
    let reply = ok(&provider, request(&["a", "b", "c"])).await;
    assert_eq!(
        reply.vectors,
        vec![vec![1.0, 1.5], vec![2.0, 2.5], vec![3.0, 3.5]]
    );
    assert_eq!(reply.usage.input_tokens, 11);
    assert_eq!(reply.usage.output_tokens, 0);
    assert_eq!(reply.model, "text-embedding-3-small");
}

/// A server that omits `index` and `usage` is read in wire order, the request's model standing
/// in for a missing one.
#[tokio::test]
async fn a_bare_reply_is_read_in_wire_order() {
    let body = r#"{"data":[{"embedding":[1.0]},{"embedding":[2.0]}]}"#;
    let provider = openai(Capture::replying(200, body));
    let reply = ok(&provider, request(&["a", "b"])).await;
    assert_eq!(reply.vectors, vec![vec![1.0], vec![2.0]]);
    assert_eq!(reply.usage.input_tokens, 0);
    assert_eq!(reply.model, "text-embedding-3-small");
}

/// Every malformed reply is `llm.protocol`, never passed on.
#[tokio::test]
async fn malformed_replies_are_protocol_errors() {
    let two = request(&["a", "b"]);
    let cases: Vec<(&str, String)> = vec![
        ("too few items", reply_of(&[(0, "[1.0]")])),
        (
            "too many items",
            reply_of(&[(0, "[1.0]"), (1, "[1.0]"), (2, "[1.0]")]),
        ),
        ("a duplicate index", reply_of(&[(0, "[1.0]"), (0, "[2.0]")])),
        (
            "a gap in the indexes",
            reply_of(&[(0, "[1.0]"), (2, "[2.0]")]),
        ),
        ("an empty vector", reply_of(&[(0, "[1.0]"), (1, "[]")])),
        (
            "different widths",
            reply_of(&[(0, "[1.0]"), (1, "[1.0,2.0]")]),
        ),
        (
            "a non-finite value",
            reply_of(&[(0, "[1.0]"), (1, "[1e39]")]),
        ),
        ("a NaN literal", reply_of(&[(0, "[1.0]"), (1, "[NaN]")])),
        ("a null value", reply_of(&[(0, "[1.0]"), (1, "[null]")])),
        (
            "mixed indexed and unindexed items",
            r#"{"data":[{"index":0,"embedding":[1.0]},{"embedding":[2.0]}]}"#.to_owned(),
        ),
        ("no data", r#"{"object":"list"}"#.to_owned()),
        ("not JSON", "<html>bad gateway</html>".to_owned()),
    ];
    for (what, body) in cases {
        let provider = openai(Capture::replying(200, &body));
        let error = err(&provider, two.clone()).await;
        assert_eq!(error.code(), "llm.protocol", "{what}: {error}");
        assert!(!error.retryable(), "{what}");
    }
}

/// 401 is `llm.auth`; 429 is rate limited with its `retry-after`; 5xx is retryable.
#[tokio::test]
async fn http_failures_map_like_a_completion() {
    let auth = openai(Capture::replying(
        401,
        r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#,
    ));
    assert_eq!(err(&auth, request(&["a"])).await, LlmError::Auth);

    let limited = openai(Capture::replying_with(
        429,
        vec![("retry-after".to_owned(), "7".to_owned())],
        r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
    ));
    let error = err(&limited, request(&["a"])).await;
    assert_eq!(
        error,
        LlmError::RateLimited {
            retry_after: Some(7)
        }
    );
    assert!(error.retryable());

    let broken = openai(Capture::replying(500, "oops"));
    let error = err(&broken, request(&["a"])).await;
    assert_eq!(error, LlmError::Overloaded);
    assert!(error.retryable());

    // An error object under a 200 status maps like a failed status.
    let hidden = openai(Capture::replying(
        200,
        r#"{"error":{"message":"bad key","type":"authentication_error"}}"#,
    ));
    assert_eq!(err(&hidden, request(&["a"])).await, LlmError::Auth);
}

/// A transport failure keeps its meaning.
#[tokio::test]
async fn transport_failures_keep_their_meaning() {
    let slow = openai(Capture::failing(TransportError::Timeout));
    assert_eq!(err(&slow, request(&["a"])).await, LlmError::Timeout);
    let down = openai(Capture::failing(TransportError::Connection(
        "refused by https://embeddings.test".to_owned(),
    )));
    assert_eq!(err(&down, request(&["a"])).await, LlmError::Unavailable);
}

/// A profile without embeddings is refused with the fixed code and sends nothing.
#[tokio::test]
async fn a_profile_without_embeddings_is_refused_without_a_call() {
    let transport = Capture::replying(200, &reply_of(&[(0, "[1.0]")]));
    let profile = Profile {
        supports_embeddings: false,
        ..Profile::generic()
    };
    let provider = build(profile, Some(KEY), transport.clone());
    let error = err(&provider, request(&["a"])).await;
    assert_eq!(error, LlmError::EmbeddingsUnsupported);
    assert_eq!(error.code(), "llm.embeddings_unsupported");
    assert!(transport.requests().is_empty());
}

/// Bounds are checked before anything is sent; a bearer provider without a key fails fast.
#[tokio::test]
async fn bounds_and_missing_keys_send_nothing() {
    let transport = Capture::replying(200, &reply_of(&[(0, "[1.0]")]));
    let provider_with_key = openai(transport.clone());
    let too_many = EmbedRequest::new("m", vec!["x".to_owned(); 257]);
    assert_eq!(
        err(&provider_with_key, too_many).await.code(),
        "llm.bad_request"
    );
    let too_big = EmbedRequest::new("m", vec!["x".repeat(32_769)]);
    assert_eq!(
        err(&provider_with_key, too_big).await.code(),
        "llm.bad_request"
    );
    assert!(transport.requests().is_empty());

    let keyless = build(Profile::generic(), None, transport.clone());
    assert_eq!(err(&keyless, request(&["a"])).await, LlmError::Auth);
    assert!(transport.requests().is_empty());
}

/// The embeddings variant of the no-leak rule: no error and no `Debug` output carries the key
/// or an input text, whatever failed.
#[tokio::test]
async fn nothing_secret_leaks_into_an_error() {
    let bodies = [
        (
            401,
            r#"{"error":{"message":"nope","type":"invalid_request_error"}}"#.to_owned(),
        ),
        (429, r#"{"error":{"type":"rate_limit_error"}}"#.to_owned()),
        (500, "internal".to_owned()),
        (200, reply_of(&[(0, "[1.0]")])),
        (200, "not json".to_owned()),
    ];
    for (status, body) in bodies {
        let provider = openai(Capture::replying(status, &body));
        let error = err(&provider, request(&[TEXT, "second"])).await;
        for printed in [error.to_string(), format!("{error:?}")] {
            assert!(!printed.contains(KEY), "{status}: {printed}");
            assert!(!printed.contains(TEXT), "{status}: {printed}");
        }
        let shown = format!("{provider:?}");
        assert!(!shown.contains(KEY), "{shown}");
    }
}

/// The same exchange replayed from a recording through the shared `MockTransport`, which
/// matches on method and URL: the recorded bytes parse like a live reply.
#[tokio::test]
async fn a_recorded_exchange_replays() {
    let recorded = RecordedExchange {
        request: HttpRequest::post_json(
            URL,
            r#"{"model":"text-embedding-3-small","input":["alpha"],"encoding_format":"float"}"#,
        ),
        response: RecordedResponse::Complete {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: reply_of(&[(0, "[0.125,-0.5,0.75]")]),
        },
    };
    let provider = OpenAiProvider::new(
        Profile::generic(),
        BASE,
        Some(ApiKey::new(KEY)),
        Arc::new(MockTransport::new(vec![recorded])),
        Vec::new(),
    );
    let reply = ok(&provider, request(&["alpha"])).await;
    assert_eq!(reply.vectors, vec![vec![0.125, -0.5, 0.75]]);
    assert_eq!(reply.width(), 3);
}

/// The shipped profiles all embed, at the shared route.
#[test]
fn the_shipped_profiles_support_embeddings() {
    for profile in [Profile::generic(), Profile::openrouter(), Profile::ollama()] {
        assert!(profile.supports_embeddings);
        assert_eq!(profile.embeddings_path, "/v1/embeddings");
    }
}
