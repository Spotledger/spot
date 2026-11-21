//! The egress-guarded transport: every request and redirect hop is authorised, only pinned
//! addresses are dialled, and the API key never reaches an error value.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use spot_core::SystemClock;
use spot_egress::{
    Authorized, EgressError, EgressPolicy, Guard, HttpRequest as WireRequest, InMemorySink,
    Purpose, Response, Sender, TestResolver,
};
use spot_llm::transport::egress::EgressTransport;
use spot_llm::transport::{HttpMethod, HttpRequest, HttpTransport, TransportError};

const HOST: &str = "api.model.test";
const PUBLIC: &str = "93.184.216.34";
const PRIVATE: &str = "10.1.2.3";
const METADATA: &str = "169.254.169.254";
const KEY: &str = "sk-live-do-not-leak-0123456789";

/// The tenant may reach the model host over HTTPS for the `llm` purpose and nothing else; the
/// guard still checks every resolved address against the public-address rule.
fn policy() -> EgressPolicy {
    EgressPolicy::from_toml(
        r#"
[tenants.acme]
allow = [{ scheme = "https", host = "api.model.test", ports = [443], purpose = "llm" }]
"#,
    )
    .unwrap()
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn sock(text: &str) -> SocketAddr {
    SocketAddr::new(ip(text), 443)
}

/// What the scripted sender saw: the URL and the pinned addresses it was told to dial.
#[derive(Clone, Debug)]
struct Seen {
    url: String,
    addrs: Vec<SocketAddr>,
    method: String,
    has_key: bool,
}

/// A sender that records its calls and answers from a script; it dials nothing.
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<Result<Response, EgressError>>>,
    seen: Mutex<Vec<Seen>>,
    delay: Option<Duration>,
}

impl Script {
    fn push(&self, answer: Result<Response, EgressError>) -> &Self {
        self.answers.lock().unwrap().push_back(answer);
        self
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sender for Script {
    async fn send_request(
        &self,
        request: &WireRequest,
        authorized: &Authorized,
        _max_body: u64,
    ) -> Result<Response, EgressError> {
        self.seen.lock().unwrap().push(Seen {
            url: request.url.to_string(),
            addrs: authorized.socket_addrs().to_vec(),
            method: request.method.as_str().to_owned(),
            has_key: request
                .headers
                .get(header::AUTHORIZATION)
                .is_some_and(|v| v.as_bytes().ends_with(KEY.as_bytes())),
        });
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(EgressError::ResolutionFailed))
    }
}

fn reply(status: u16, body: &str) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap(),
        headers: HeaderMap::new(),
        body: body.as_bytes().to_vec(),
    }
}

fn redirect(status: u16, location: &str) -> Response {
    let mut response = reply(status, "");
    response
        .headers
        .insert(header::LOCATION, HeaderValue::from_str(location).unwrap());
    response
}

struct Rig {
    resolver: Arc<TestResolver>,
    script: Arc<Script>,
    transport: EgressTransport,
}

/// A transport whose resolver answers `answers` in turn for [`HOST`].
fn rig_with(answers: Vec<Vec<IpAddr>>, script: Script) -> Rig {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(HOST, answers);
    let guard = Arc::new(Guard::new(
        policy(),
        resolver.clone(),
        Arc::new(InMemorySink::new()),
        Arc::new(SystemClock::new()),
    ));
    let script = Arc::new(script);
    let transport = EgressTransport::new(guard, script.clone(), "acme", Purpose::Llm);
    Rig {
        resolver,
        script,
        transport,
    }
}

fn new_rig(answers: Vec<Vec<IpAddr>>) -> Rig {
    rig_with(answers, Script::default())
}

fn call(url: &str) -> HttpRequest {
    HttpRequest::post_json(url, r#"{"model":"m"}"#)
        .with_header("authorization", format!("Bearer {KEY}"))
}

fn endpoint() -> String {
    format!("https://{HOST}/v1/chat?key={KEY}")
}

/// No error text or debug form of `error` may carry the key or the URL query.
fn assert_clean(error: &TransportError) {
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("key="), "{text}");
        assert!(!text.contains(HOST), "{text}");
    }
}

#[tokio::test]
async fn an_endpoint_that_resolves_to_a_private_address_is_refused_with_no_connection() {
    for private in [PRIVATE, METADATA, "127.0.0.1"] {
        let rig = new_rig(vec![vec![ip(private)]]);
        let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
        assert!(
            matches!(&error, TransportError::Protocol(text) if text.contains("egress.denied_address")),
            "{private}: {error:?}"
        );
        assert!(!error.retryable());
        assert_clean(&error);
        assert!(rig.script.seen().is_empty(), "{private}: nothing was sent");
        // The streaming path is guarded the same way.
        let rig = rig_with(vec![vec![ip(private)]], Script::default());
        assert!(rig.transport.send_stream(call(&endpoint())).await.is_err());
        assert!(rig.script.seen().is_empty());
    }
}

#[tokio::test]
async fn a_host_that_rebinds_after_the_check_never_reaches_the_private_address() {
    // Public on the first lookup, private on the second.
    let rig = new_rig(vec![vec![ip(PUBLIC)], vec![ip(PRIVATE)]]);
    rig.script.push(Ok(reply(200, "{}")));

    let ok = rig.transport.send(call(&endpoint())).await.unwrap();
    assert_eq!(ok.status, 200);
    let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
    assert_clean(&error);

    // Exactly one send, to the vetted public address only; the transport never looked up the
    // name itself (the guard did, twice).
    let seen = rig.script.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].addrs, vec![sock(PUBLIC)]);
    assert!(seen[0].has_key, "the key travels to the vetted address");
    assert_eq!(rig.resolver.call_count(HOST), 2);
}

#[tokio::test]
async fn a_redirect_to_a_private_address_is_refused() {
    // Another origin: refused before it is even resolved.
    let rig = new_rig(vec![vec![ip(PUBLIC)]]);
    rig.script.push(Ok(redirect(
        302,
        &format!("https://{METADATA}/latest/meta-data"),
    )));
    let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
    assert!(
        matches!(&error, TransportError::Protocol(text) if text.contains("another origin")),
        "{error:?}"
    );
    assert_clean(&error);
    assert_eq!(
        rig.script.seen().len(),
        1,
        "the redirect target was never sent to"
    );

    // The same origin whose name now resolves to a private address: the hop is authorised
    // again and refused, and nothing is sent to the private address.
    let rig = new_rig(vec![vec![ip(PUBLIC)], vec![ip(PRIVATE)]]);
    rig.script.push(Ok(redirect(307, "/v2/chat")));
    let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
    assert!(
        matches!(&error, TransportError::Protocol(text) if text.contains("egress.denied_address")),
        "{error:?}"
    );
    assert_clean(&error);
    let seen = rig.script.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].addrs, vec![sock(PUBLIC)]);

    // A downgrade to plain HTTP on the same host is another origin.
    let rig = new_rig(vec![vec![ip(PUBLIC)]]);
    rig.script
        .push(Ok(redirect(301, &format!("http://{HOST}/v1/chat"))));
    assert!(rig.transport.send(call(&endpoint())).await.is_err());
    assert_eq!(rig.script.seen().len(), 1);
}

#[tokio::test]
async fn a_same_origin_redirect_is_followed_and_authorised_again() {
    let rig = new_rig(vec![vec![ip(PUBLIC)], vec![ip(PUBLIC)]]);
    rig.script.push(Ok(redirect(307, "/v2/chat")));
    rig.script.push(Ok(reply(200, r#"{"ok":true}"#)));
    let response = rig.transport.send(call(&endpoint())).await.unwrap();
    assert_eq!(response.body, r#"{"ok":true}"#);
    let seen = rig.script.seen();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].url.ends_with("/v2/chat"));
    assert_eq!(seen[1].method, "POST", "a 307 keeps the method");
    assert_eq!(rig.resolver.call_count(HOST), 2, "every hop is authorised");

    // A 302 becomes a GET.
    let rig = new_rig(vec![vec![ip(PUBLIC)], vec![ip(PUBLIC)]]);
    rig.script.push(Ok(redirect(302, "/v2/chat")));
    rig.script.push(Ok(reply(200, "{}")));
    rig.transport.send(call(&endpoint())).await.unwrap();
    assert_eq!(rig.script.seen()[1].method, "GET");
}

#[tokio::test]
async fn redirects_are_bounded() {
    let rig = new_rig(vec![vec![ip(PUBLIC)]; 8]);
    for _ in 0..8 {
        rig.script.push(Ok(redirect(307, "/again")));
    }
    let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
    assert!(matches!(&error, TransportError::Protocol(text) if text.contains("too many")));
    assert_eq!(
        rig.script.seen().len(),
        4,
        "the first send and three redirects"
    );
}

#[tokio::test]
async fn no_error_value_carries_the_key_or_the_url() {
    let failures = [
        EgressError::ResolutionFailed,
        EgressError::PolicyDenied,
        EgressError::PortDenied { port: 22 },
        EgressError::ResolutionMixed,
        EgressError::RequestTooLarge { limit: 1 },
        EgressError::InvalidUrl,
    ];
    for failure in failures {
        let rig = new_rig(vec![vec![ip(PUBLIC)]]);
        rig.script.push(Err(failure.clone()));
        let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
        assert_clean(&error);
        let rig = rig_with(vec![vec![ip(PUBLIC)]], Script::default());
        rig.script.push(Err(failure));
        let error = rig
            .transport
            .send_stream(call(&endpoint()))
            .await
            .err()
            .unwrap();
        assert_clean(&error);
    }
    // And a malformed request.
    let rig = new_rig(vec![]);
    let mut bad = call("not a url");
    bad.headers.push(("bad name".to_owned(), KEY.to_owned()));
    assert_clean(&rig.transport.send(bad).await.unwrap_err());
    let mut head = call(&endpoint());
    head.method = HttpMethod::Head;
    assert_clean(&rig.transport.send(head).await.unwrap_err());
}

#[tokio::test]
async fn a_buffered_read_is_capped_and_a_stream_arrives_in_chunks() {
    let rig = new_rig(vec![vec![ip(PUBLIC)]]);
    rig.script.push(Err(EgressError::ResponseTooLarge));
    let error = rig.transport.send(call(&endpoint())).await.unwrap_err();
    assert!(
        matches!(error, TransportError::TooLarge { .. }),
        "{error:?}"
    );

    let rig = new_rig(vec![vec![ip(PUBLIC)]]);
    rig.script.push(Ok(reply(200, &"x".repeat(40_000))));
    let mut stream = rig.transport.send_stream(call(&endpoint())).await.unwrap();
    let mut total = 0;
    let mut chunks = 0;
    while let Some(chunk) = futures_next(&mut stream).await {
        total += chunk.unwrap().len();
        chunks += 1;
    }
    assert_eq!(total, 40_000);
    assert!(chunks > 1);
}

async fn futures_next(
    stream: &mut spot_llm::transport::ByteStream,
) -> Option<Result<Vec<u8>, TransportError>> {
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)).await
}

#[tokio::test(start_paused = true)]
async fn the_request_timeout_bounds_the_whole_exchange() {
    let rig = rig_with(
        vec![vec![ip(PUBLIC)]],
        Script {
            delay: Some(Duration::from_secs(60)),
            ..Script::default()
        },
    );
    rig.script.push(Ok(reply(200, "{}")));
    let request = call(&endpoint()).with_timeout(Duration::from_secs(5));
    let error = rig.transport.send(request).await.unwrap_err();
    assert_eq!(error, TransportError::Timeout);
    assert!(error.retryable());
}

#[tokio::test]
async fn the_production_constructor_authorises_before_it_connects() {
    // No scripted sender: the real HTTP sender sits behind the guard, and a private answer is
    // refused before any connection is attempted.
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(HOST, vec![vec![ip(PRIVATE)]]);
    let guard = Arc::new(Guard::new(
        policy(),
        resolver.clone(),
        Arc::new(InMemorySink::new()),
        Arc::new(SystemClock::new()),
    ));
    let transport = EgressTransport::for_tenant(guard, "acme", Duration::from_secs(5));
    let error = transport.send(call(&endpoint())).await.unwrap_err();
    assert!(
        matches!(&error, TransportError::Protocol(text) if text.contains("egress.denied_address")),
        "{error:?}"
    );
    assert_clean(&error);
}
