//! Shared support for the native realtime WebSocket suites (WP-PLT-08.T2): the server
//! harness (the spot-http server with the realtime route mounted as its site routes), the
//! fake token verifier with revocation watching, the WebSocket client, and the
//! schema-conformance check every server message goes through.
//!
//! Each suite declares `mod support;` privately; `dead_code` is allowed at the declaration
//! because a suite uses only the part of the support tree its name implies.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value as Json;
use spot_core::{SiteId, SystemClock};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_realtime::ws::{ExpiryWatch, Rejected, TokenVerifier, WsConfig};
use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus, RoomAuthorizer};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as WireMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;

/// The pieces suites reach through `support::`: the message types and the subprotocol.
pub use spot_realtime::ws::{ClientMessage, SUBPROTOCOL};

/// Test-only unwrapping that names the failure (`unwrap_used`/`expect_used` are denied by
/// the workspace lints, also for tests).
pub trait Must<T> {
    /// Unwraps or panics with the error in a named failure message.
    fn must(self) -> T;
}

impl<T, E> Must<T> for Result<T, E>
where
    E: std::fmt::Debug,
{
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}

// ---------------------------------------------------------------------------
// Schema conformance: every server message the tests read validates against
// `RealtimeServerMessage` in `contracts/native-api/openapi.yaml`.

/// The contract document the tests validate server messages against.
const CONTRACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../contracts/native-api/openapi.yaml"
);

/// The base URI the contract document is registered under for `$ref` assembly.
const BASE: &str = "https://spotledger.test/native-api/v0.1";

/// The parsed contract document, read once.
fn contract_root() -> &'static Json {
    static ROOT: OnceLock<Json> = OnceLock::new();
    ROOT.get_or_init(|| {
        let text = std::fs::read_to_string(CONTRACT)
            .unwrap_or_else(|error| panic!("read the contract: {error}"));
        serde_norway::from_str(&text).unwrap_or_else(|error| panic!("parse the contract: {error}"))
    })
}

/// The `RealtimeServerMessage` validator, compiled once.
fn server_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let registry = jsonschema::Registry::new()
            .add(BASE, contract_root().clone())
            .must()
            .prepare()
            .must();
        let schema = serde_json::json!({"$ref": format!("{BASE}#/components/schemas/RealtimeServerMessage")});
        jsonschema::options()
            .with_registry(&registry)
            .build(&schema)
            .must()
    })
}

/// Asserts that one server message matches the contract's `RealtimeServerMessage` schema.
/// Every server message the tests read goes through here.
pub fn validate_server_message(message: &Json) {
    let errors: Vec<String> = server_validator()
        .iter_errors(message)
        .map(|error| format!("{} (at `{}`)", error, error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "server message is not schema-conformant: {message}\nerrors: {}",
        errors.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The fake token verifier: admits tokens a test registers, and can revoke them (the
// transport's expiry watcher then asks again and closes 4009).

/// The token → user-id map, with a revocation signal. The user ids are wire-valid
/// (`RecordId`-shaped) because `ready.user` and the `user:<id>` room are validated against
/// the contract by the conformance harness.
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

struct Session {
    user: String,
    revoke: Arc<Notify>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl SessionStore {
    /// Registers a token that verifies to the user id. The revocation signal is reused
    /// across re-admissions of the same token, so a watcher's signal line survives a
    /// permission change that keeps the session.
    pub fn admit(&self, token: &str, user: &str) {
        let mut sessions = lock(&self.sessions);
        let revoke = sessions
            .get(token)
            .map(|session| Arc::clone(&session.revoke))
            .unwrap_or_else(|| Arc::new(Notify::new()));
        sessions.insert(
            token.to_owned(),
            Session {
                user: user.to_owned(),
                revoke,
            },
        );
    }

    /// Revokes a live session: the transport's watcher sees the signal, re-verifies, fails,
    /// and the connection closes 4009. Covers every watching connection that has registered
    /// (notify_waiters) or registers right after (notify_one stores one permit).
    pub fn revoke(&self, token: &str) {
        let session = lock(&self.sessions).remove(token);
        if let Some(session) = session {
            session.revoke.notify_waiters();
            session.revoke.notify_one();
        }
    }

    /// Fires a session's revocation signal *without* removing it — the transport's watcher
    /// wakes, re-verifies, finds the session still valid, and keeps watching (a permission
    /// change the verifier tolerates looks like this).
    pub fn signal(&self, token: &str) {
        let session = lock(&self.sessions)
            .get(token)
            .map(|session| session.revoke.clone());
        if let Some(session) = session {
            session.notify_waiters();
            session.notify_one();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct SessionWatch {
    sessions: Arc<SessionStore>,
    token: String,
    revoke: Arc<Notify>,
}

impl ExpiryWatch for SessionWatch {
    /// Resolves when the token disappears from the store after a signal on its revocation;
    /// a signal without a removal (say, a permission change) keeps the watch waiting.
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let sessions = Arc::clone(&self.sessions);
        let token = self.token.clone();
        let revoke = Arc::clone(&self.revoke);
        Box::pin(async move {
            loop {
                revoke.notified().await;
                if !lock(&sessions.sessions).contains_key(&token) {
                    return;
                }
            }
        })
    }
}

/// The verifier the transport talks to. `verify` is a map lookup: a token the store admits
/// verifies to its user id, anything else is rejected.
pub struct FakeVerifier {
    sessions: Arc<SessionStore>,
}

impl TokenVerifier for FakeVerifier {
    fn verify<'a>(
        &'a self,
        _site: &'a spot_core::SiteId,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Principal, Rejected>> + Send + 'a>> {
        Box::pin(async move {
            lock(&self.sessions.sessions)
                .get(token)
                .map(|session| Principal::user(session.user.clone()))
                .ok_or_else(|| Rejected::new("no such session"))
        })
    }

    fn expiry_watch(&self, token: &str) -> Option<Arc<dyn ExpiryWatch>> {
        let revoke = lock(&self.sessions.sessions)
            .get(token)
            .map(|session| session.revoke.clone())?;
        Some(Arc::new(SessionWatch {
            sessions: self.sessions.clone(),
            token: token.to_owned(),
            revoke,
        }))
    }
}

// ---------------------------------------------------------------------------
// The server harness: the real spot-http server assembly with the realtime router mounted
// as its site routes, served on a loopback listener.

/// A user id in the wire (`RecordId`) shape, used for test principals.
pub const TEST_USER: &str = "0192f3c4-5b6a-7c8d-9e0f-00000000aaaa";

/// The running server plus everything a test talks to it with.
pub struct Harness {
    /// The site the server serves (by its own name, like a real deployment's default host).
    pub site: SiteId,
    /// The bus events are published through.
    pub bus: RealtimeBus,
    /// The token store the verifier reads; tests admit and revoke sessions on it.
    pub store: Arc<SessionStore>,
    /// The server-shutdown signal: cancelling it closes every connection with 1001.
    pub shutdown: spot_realtime::ws::CancellationToken,
    /// The loopback address the server listens on.
    pub addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Starts a server with the defaults (heartbeat 25 s, `AllowAll`, the bus's queue capacity)
/// and `TEST_USER` admitted under `token:test`.
pub async fn harness() -> Harness {
    let mut harness = start(25, Arc::new(AllowAll), 256).await;
    harness.admit(TEST_TOKEN, TEST_USER);
    harness
}

/// The token `harness()` admits by default.
pub const TEST_TOKEN: &str = "token:test";

/// Starts a server with explicit heartbeat cadence, authorizer, and bus queue capacity.
pub async fn start(
    heartbeat_seconds: u64,
    authorizer: Arc<dyn RoomAuthorizer>,
    queue_capacity: usize,
) -> Harness {
    let store = Arc::new(SessionStore::default());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(FakeVerifier {
        sessions: Arc::clone(&store),
    });
    start_with_verifier(
        verifier,
        store,
        heartbeat_seconds,
        authorizer,
        queue_capacity,
    )
    .await
}

/// Like [`start`] with a caller-supplied verifier (the async-verifier suite's slow, gated and
/// re-rejecting verifiers). `store` is only the harness's own admit/revoke handle.
pub async fn start_with_verifier(
    verifier: Arc<dyn TokenVerifier>,
    store: Arc<SessionStore>,
    heartbeat_seconds: u64,
    authorizer: Arc<dyn RoomAuthorizer>,
    queue_capacity: usize,
) -> Harness {
    let site = SiteId::new("acme.example").unwrap_or_else(|e| panic!("site: {e}"));
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        authorizer,
    );
    let shutdown = spot_realtime::ws::CancellationToken::new();
    // The site resolves by its name (the deployment's default host) and by the loopback
    // host the ws test client sends (`Host: 127.0.0.1:<port>`).
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add("127.0.0.1", site.clone())
            .add_site_name(site.clone()),
    )
    .site_routes(spot_realtime::ws::router(
        bus.clone(),
        verifier,
        WsConfig {
            heartbeat_seconds,
            shutdown: shutdown.clone(),
        },
    ))
    .build();
    let listener = TcpListener::bind("127.0.0.1:0").await.must();
    let addr = listener.local_addr().must();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .unwrap_or_else(|e| panic!("server failed: {e}"));
    });
    Harness {
        site,
        bus,
        store,
        shutdown,
        addr,
        server,
    }
}

impl Harness {
    /// Admits a session token that verifies to a user id.
    pub fn admit(&mut self, token: &str, user: &str) -> &mut Self {
        self.store.admit(token, user);
        self
    }

    /// Revokes a live session (the transport closes its connection with 4009).
    pub fn revoke(&self, token: &str) {
        self.store.revoke(token);
    }

    /// Fires a session signal without removing it (a tolerated permission change).
    pub fn signal(&self, token: &str) {
        self.store.signal(token);
    }
}

// ---------------------------------------------------------------------------
// The WebSocket client: real sockets. `futures-util` is not a permitted dependency, so the
// client speaks through `tungstenite`'s blocking API on the blocking pool; the server side
// is the async axum stack, exactly as deployed.

use tokio_tungstenite::tungstenite;

/// A connected, blocking WebSocket client on the site's realtime route.
#[derive(Debug)]
pub struct Client {
    inner: Arc<Mutex<tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>>>,
}

/// Bounds a blocking read/write the test runner would otherwise wait on forever when the
/// server fails to answer. Generous enough for a real wall-clock wait on the contract's
/// ten-second auth window (the close must be observable, not eaten by this bound).
const IO_TIMEOUT: Duration = Duration::from_secs(12);

fn request(
    addr: std::net::SocketAddr,
    subprotocol: Option<&str>,
) -> tungstenite::http::Request<()> {
    let mut request = format!("ws://{addr}/api/v1/realtime")
        .into_client_request()
        .must();
    if let Some(name) = subprotocol {
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", name.parse().must());
    }
    request
}

/// Connects, optionally offering a subprotocol. The raw result, so a test can assert the
/// refusal envelope the server answers with (no WebSocket is established).
pub async fn connect_raw(
    addr: std::net::SocketAddr,
    subprotocol: Option<&str>,
) -> Result<Client, tungstenite::Error> {
    let wanted = subprotocol.map(|name| name.to_owned());
    let address = addr;
    tokio::task::spawn_blocking(move || {
        tungstenite::client::connect(request(address, wanted.as_deref()))
    })
    .await
    .must()
    .map(|(ws, _response)| {
        // A hung server must fail the test, not hang CI: bound every socket wait.
        bound_socket_timeouts(&ws);
        Client {
            inner: Arc::new(Mutex::new(ws)),
        }
    })
}

/// Connects speaking `spot.v1`; asserts the server *selected* the subprotocol (browsers
/// reject a handshake that does not echo it).
pub async fn connect(addr: std::net::SocketAddr) -> Client {
    let (client, response) = connect_with_response(addr, Some(SUBPROTOCOL)).await.must();
    assert_eq!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok()),
        Some(SUBPROTOCOL),
        "the server must select spot.v1"
    );
    client
}

/// The connect result with its handshake response, for the suites that assert headers. The
/// response body type is tungstenite's own (`http::Response<Option<Vec<u8>>>`).
pub async fn connect_with_response(
    addr: std::net::SocketAddr,
    subprotocol: Option<&str>,
) -> Result<
    (
        Client,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    let wanted = subprotocol.map(|name| name.to_owned());
    let address = addr;
    let result = tokio::task::spawn_blocking(move || {
        tungstenite::client::connect(request(address, wanted.as_deref()))
    })
    .await
    .must();
    result.map(|(ws, response)| {
        bound_socket_timeouts(&ws);
        (
            Client {
                inner: Arc::new(Mutex::new(ws)),
            },
            response,
        )
    })
}

/// Bounds the loopback socket's blocking waits so a dead server fails a test, not CI.
fn bound_socket_timeouts(ws: &tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>) {
    let stream = match ws.get_ref() {
        MaybeTlsStream::Plain(tcp) => tcp,
        // The crate is built without TLS features; the enum is `#[non_exhaustive]`, so the
        // catch-all is required and unreachable in this build.
        #[allow(unreachable_patterns)]
        _ => panic!("the tests speak plain TCP only"),
    };
    stream.set_read_timeout(Some(IO_TIMEOUT)).must();
    stream.set_write_timeout(Some(IO_TIMEOUT)).must();
}

impl Client {
    /// Writes one wire frame (blocking, bounded by the socket timeouts). Panics on a dead
    /// socket; `try_send` is the asserting variant.
    pub async fn send(&self, message: impl Into<WireMessage>) {
        self.try_send(message).await.must();
    }

    /// Writes one wire frame and reports the transport result — used where a test asserts
    /// that a closing (or closed) socket accepts nothing more.
    pub async fn try_send(
        &self,
        message: impl Into<WireMessage>,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        let inner = Arc::clone(&self.inner);
        let message = message.into();
        tokio::task::spawn_blocking(move || lock(&inner).send(message))
            .await
            .must()
    }

    /// Sends a protocol message and reports the transport result — the message form of
    /// [`try_send`](Client::try_send).
    pub async fn try_send_message(
        &self,
        message: &ClientMessage,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        let text = serde_json::to_string(message).must();
        self.try_send(text).await
    }

    /// Sends a raw WebSocket ping (a client liveness frame, not a protocol message).
    pub async fn send_ws_ping(&self, payload: &[u8]) {
        self.send(WireMessage::Ping(payload.to_vec().into())).await;
    }

    /// Sends a client message as JSON text.
    pub async fn send_message(&self, message: &ClientMessage) {
        self.send(serde_json::to_string(message).must()).await;
    }

    /// Sends a raw text frame (for the protocol-violation cases).
    pub async fn send_raw(&self, text: &str) {
        self.send(text.to_owned()).await;
    }

    /// Sends a binary frame, which the protocol forbids.
    pub async fn send_binary(&self, bytes: &[u8]) {
        self.send(WireMessage::from(bytes.to_vec())).await;
    }

    /// Closes the client's side normally (1000, no reason) — or with a frame when given.
    pub async fn close(&self, frame: Option<tungstenite::protocol::CloseFrame>) {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || lock(&inner).close(frame))
            .await
            .must()
            .must();
    }

    /// Reads the next wire frame. Text frames the server sends are validated against the
    /// contract's `RealtimeServerMessage` schema (this is how the suites fulfill the
    /// conformance requirement: nothing a test reads goes unvalidated).
    pub async fn read(&self) -> WireMessage {
        let inner = Arc::clone(&self.inner);
        let message = tokio::task::spawn_blocking(move || lock(&inner).read())
            .await
            .must()
            .unwrap_or_else(|error| panic!("read timed out or failed: {error:?}"));
        if let WireMessage::Text(text) = &message {
            let parsed: Json = serde_json::from_str(text.as_str()).must();
            validate_server_message(&parsed);
        }
        message
    }

    /// Reads server messages until the close frame; returns its code (`u16`). The messages
    /// read along the way are conformance-checked like every other read.
    pub async fn read_until_close(&self) -> u16 {
        loop {
            match self.read().await {
                WireMessage::Close(frame) => {
                    return frame.map(|frame| frame.code.into()).unwrap_or(1005);
                }
                // Heartbeat ping/pong frames and anything the server streamed first are
                // validated and skipped by `read`.
                _ => continue,
            }
        }
    }
}

/// A `Client`'s next JSON server message. Validation happens inside [`Client::read`]; a
/// close frame surfacing here is a test failure (suites assert closes through
/// [`Client::read_until_close`]).
pub async fn recv_json(client: &Client) -> Json {
    match client.read().await {
        WireMessage::Text(text) => serde_json::from_str(text.as_str()).must(),
        // Heartbeat ping/pong frames are transport liveness, not protocol messages; the
        // next protocol message follows them in stream order.
        WireMessage::Ping(_) | WireMessage::Pong(_) => Box::pin(recv_json(client)).await,
        other => panic!("expected a server message, got: {other:?}"),
    }
}

/// A `Client`'s next server message of the wanted `type` (skipping others, all validated).
pub async fn expect_kind(client: &Client, wanted: &str) -> Json {
    loop {
        let message = recv_json(client).await;
        if message.get("type").and_then(Json::as_str) == Some(wanted) {
            return message;
        }
    }
}

/// A wire-valid document room name, for room-limit and delivery tests.
pub fn doc_room_name(n: usize) -> String {
    format!("doc:items:0192f3c4-5b6a-7c8d-9e0f-{n:012x}")
}

/// Auth + `ready` in one step: connects, sends the `auth` for `token`, and consumes the
/// `ready` answer (returning the client and the `ready` message, so a suite can assert the
/// user it verified).
pub async fn connect_and_auth(harness: &Harness, token: &str) -> (Client, Json) {
    let client = connect(harness.addr).await;
    client
        .send_message(&ClientMessage::Auth {
            token: token.to_owned(),
        })
        .await;
    let ready = expect_kind(&client, "ready").await;
    (client, ready)
}
