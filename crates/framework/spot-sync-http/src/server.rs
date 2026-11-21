//! The hub side: the axum router and the three edge-initiated endpoints.
//!
//! ADR-062 section 3: the edge always dials out, so every route is a request the edge makes and
//! the hub answers; nothing here initiates a connection. The routes are:
//!
//! * `POST /sync/v1/hello` — the handshake: version negotiation and the metadata epoch.
//! * `GET  /sync/v1/pull`  — a long-poll for the next batch, with heartbeats.
//! * `POST /sync/v1/push`  — a streamed batch the hub applies.
//!
//! Identity runs first, as middleware: a [`ClientCertificate`](crate::identity::ClientCertificate)
//! must already be in the request's extensions (the TLS layer puts it there), and the
//! [`NodeDirectory`](crate::identity::NodeDirectory) must resolve it to an
//! [`AuthenticatedNode`](crate::identity::AuthenticatedNode). A request without one never reaches
//! a handler.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_sync_protocol::{Batch, Hello, NodeId, ProtocolRange, ProtocolVersion};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{Error, Failure};
use crate::hub::{HubSource, PushSink};
use crate::identity::{AuthenticatedNode, ClientCertificate, NodeDirectory};
use crate::wire::{
    self, BatchHeader, CONTENT_TYPE, HelloReply, PushReply, heartbeat_line_with, percent_encode,
};

/// The hub's supply of a pre-signed hub time for a waiting pull's heartbeats.
///
/// A node may take hub time only from a message the hub signed (`spot-offline-identity`),
/// so the value returned here is the **already signed** message — its JSON as the node will
/// verify it — and never a bare clock reading: this crate does not know what a signature is, and
/// a client that trusted the payload without verifying it would let anyone on the path move the
/// node's clock. How the payload is shaped is between the hub's signer and the node's snapshot
/// store; the transport only carries it.
///
/// `current` is called from the pull's streaming task, once per heartbeat, **outside any lock**,
/// on the request path: it must be cheap and must not sign per call (sign once, reuse, or hand
/// out a cached signed time). It is asked for the name the node directory resolved for this
/// authenticated request, so a hub can sign a time bound to that node.
pub trait HubTimeSource: Send + Sync {
    /// The signed hub time to carry in a heartbeat for `node`, or `None` to send a bare one.
    fn current(&self, node: &str) -> Option<serde_json::Value>;
}

/// The hub's limits and timings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerSettings {
    /// The most records one batch may carry.
    pub max_batch_records: usize,
    /// The most bytes one push body may carry.
    pub max_push_bytes: usize,
    /// The longest a pull holds the connection open waiting for records.
    pub long_poll: Duration,
    /// How often a waiting pull writes a heartbeat line.
    pub heartbeat: Duration,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            max_batch_records: 4_096,
            max_push_bytes: wire::MAX_BODY_BYTES,
            long_poll: Duration::from_secs(25),
            heartbeat: Duration::from_secs(5),
        }
    }
}

/// The hub: its identity, the directory, the stream it serves and the sink it applies to.
#[derive(Clone)]
pub struct SyncServer {
    hub_node: NodeId,
    protocol_range: ProtocolRange,
    metadata_epoch: u64,
    directory: Arc<dyn NodeDirectory>,
    hub: Arc<dyn HubSource>,
    sink: Arc<dyn PushSink>,
    clock: Arc<dyn Clock>,
    settings: ServerSettings,
    hub_time: Option<Arc<dyn HubTimeSource>>,
}

impl std::fmt::Debug for SyncServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncServer")
            .field("hub_node", &self.hub_node)
            .field("protocol_range", &self.protocol_range)
            .field("metadata_epoch", &self.metadata_epoch)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl SyncServer {
    /// Assembles a hub.
    pub fn new(
        hub_node: NodeId,
        directory: Arc<dyn NodeDirectory>,
        hub: Arc<dyn HubSource>,
        sink: Arc<dyn PushSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            hub_node,
            protocol_range: ProtocolRange::current(),
            metadata_epoch: 0,
            directory,
            hub,
            sink,
            clock,
            settings: ServerSettings::default(),
            hub_time: None,
        }
    }

    /// Sets the limits and timings.
    pub fn with_settings(mut self, settings: ServerSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Sets the source of the pre-signed hub time a waiting pull's heartbeats may carry.
    ///
    /// The source is a supply of values, not a policy: without one (or when it answers `None`)
    /// every heartbeat is today's bare `{"heartbeat":true}` line, byte for byte.
    pub fn with_hub_time_source(mut self, source: Arc<dyn HubTimeSource>) -> Self {
        self.hub_time = Some(source);
        self
    }

    /// Sets the protocol range and metadata epoch this hub advertises.
    pub fn with_identity(mut self, protocol_range: ProtocolRange, metadata_epoch: u64) -> Self {
        self.protocol_range = protocol_range;
        self.metadata_epoch = metadata_epoch;
        self
    }

    /// The hub's settings.
    pub fn settings(&self) -> ServerSettings {
        self.settings
    }

    /// The router, ready to mount on any listener (plain, or TLS with the identity extension).
    pub fn router(self) -> Router {
        let state = Arc::new(self);
        Router::new()
            .route("/sync/v1/hello", post(hello))
            .route("/sync/v1/pull", get(pull))
            .route("/sync/v1/push", post(push))
            .with_state(Arc::clone(&state))
            .layer(middleware::from_fn_with_state(state, require_identity))
    }
}

/// Resolves the presented client certificate to a node, or refuses the request.
///
/// The certificate is put in the extensions by the TLS layer (or the loopback transport). A
/// request without one is `sync_http.identity_required`; one whose certificate the directory does not
/// know is `sync_http.identity_unknown`; a revoked one is `sync_http.identity_revoked`.
async fn require_identity(
    State(server): State<Arc<SyncServer>>,
    mut request: Request,
    next: Next,
) -> Response {
    let certificate = request.extensions().get::<ClientCertificate>().cloned();
    let Some(certificate) = certificate else {
        return Failure::from(Error::identity_required(
            "no client certificate was presented",
        ))
        .into_response();
    };
    match server.directory.resolve(&certificate) {
        Ok(node) => {
            tracing::debug!(node = %node.node, "authenticated sync request");
            request.extensions_mut().insert(node);
            next.run(request).await
        }
        Err(error) => Failure::from(error).into_response(),
    }
}

/// `POST /sync/v1/hello`: negotiate a protocol version and check the metadata epoch.
async fn hello(
    State(server): State<Arc<SyncServer>>,
    Extension(_node): Extension<AuthenticatedNode>,
    request: Request,
) -> std::result::Result<axum::Json<HelloReply>, Failure> {
    let body = read_body(request, wire::MAX_BODY_BYTES).await?;
    let theirs: Hello = serde_json::from_slice(&body).map_err(|error| {
        Error::invalid_request(format!("the hello body did not parse: {error}"))
    })?;
    let ours = Hello::new(
        server.hub_node.clone(),
        server.protocol_range,
        server.metadata_epoch,
    );
    let protocol: ProtocolVersion = ours
        .negotiate(&theirs)
        .map_err(|_| Error::version_mismatch(server.protocol_range, theirs.protocol_range))?;
    if !ours.same_epoch(&theirs) {
        return Err(Error::metadata_epoch(server.metadata_epoch, theirs.metadata_epoch).into());
    }
    Ok(axum::Json(HelloReply {
        protocol,
        hub: ours,
    }))
}

/// `GET /sync/v1/pull`: a long-poll for the next batch.
///
/// The response is an NDJSON stream: the batch's header line and its record lines, or heartbeat
/// lines while the hub waits for records. The wait is capped by [`ServerSettings::long_poll`],
/// so an idle stream still returns and the client re-polls. A heartbeat may carry a pre-signed
/// hub time from the server's [`HubTimeSource`], bound to the node the directory authenticated.
async fn pull(
    State(server): State<Arc<SyncServer>>,
    Extension(node): Extension<AuthenticatedNode>,
    Query(params): Query<BTreeMap<String, String>>,
) -> std::result::Result<Response, Failure> {
    let source = parse_source(&params)?;
    if !node.may_pull(&source, server.hub_node.site()) {
        return Err(Error::partition_violation(node.node.as_str(), source.as_str()).into());
    }
    let from_seq = parse_u64(&params, "from_seq")?.max(1);
    let requested = params
        .get("max")
        .map(|value| parse_usize(value, "max"))
        .transpose()?
        .unwrap_or(server.settings.max_batch_records);
    let max = requested.min(server.settings.max_batch_records).max(1);

    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(4);
    let task_server = server;
    let task_source = source.clone();
    // The caller's node, as the directory resolved it for this request: the only name a hub time
    // may be issued to. Nothing from the body, the query or a header is used.
    let caller = node.node.clone();
    tokio::spawn(async move {
        pump_pull(task_server, task_source, caller, from_seq, max, tx).await;
    });

    let body = Body::from_stream(ReceiverStream::new(rx));
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response())
}

/// Waits for records and streams the batch, or heartbeats until the long-poll cap.
async fn pump_pull(
    server: Arc<SyncServer>,
    source: SourceId,
    caller: NodeId,
    from_seq: u64,
    max: usize,
    tx: mpsc::Sender<std::result::Result<Bytes, Infallible>>,
) {
    if send_batch_if_available(&server, &source, from_seq, max, &tx).await {
        return;
    }
    let deadline = server
        .clock
        .monotonic()
        .saturating_add(server.settings.long_poll);
    loop {
        let remaining = deadline.saturating_sub(server.clock.monotonic());
        if remaining.is_zero() {
            return;
        }
        let wait = remaining.min(server.settings.heartbeat);
        tokio::select! {
            () = server.hub.changed(&source, from_seq) => {
                if send_batch_if_available(&server, &source, from_seq, max, &tx).await {
                    return;
                }
            }
            () = tokio::time::sleep(wait) => {
                // Asked per heartbeat and outside any lock: the source hands out a value it has
                // already signed, it never signs on this path.
                let payload = server
                    .hub_time
                    .as_ref()
                    .and_then(|hub_time| hub_time.current(caller.as_str()));
                if tx.send(Ok(heartbeat_line_with(payload.as_ref()))).await.is_err() {
                    // The client went away; the handler's body was dropped.
                    return;
                }
            }
        }
    }
}

/// Reads the hub and sends the batch, returning whether anything was sent.
async fn send_batch_if_available(
    server: &SyncServer,
    source: &SourceId,
    from_seq: u64,
    max: usize,
    tx: &mpsc::Sender<std::result::Result<Bytes, Infallible>>,
) -> bool {
    let records = match server.hub.read(source, from_seq, max) {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(code = error.code, "hub read failed; closing the pull");
            return true;
        }
    };
    if records.is_empty() {
        return false;
    }
    let batch = match Batch::new(source.clone(), records) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(code = error.code, "the hub served a non-contiguous run");
            return true;
        }
    };
    let body = match wire::encode_batch(&batch) {
        Ok(body) => body,
        Err(_) => return true,
    };
    tx.send(Ok(body)).await.is_ok()
}

/// `POST /sync/v1/push`: apply a streamed batch.
///
/// The header is read and checked first: a batch whose source partition the authenticated node
/// does not own is refused with `sync_http.partition_violation` **before any record is decoded**
/// (ADR-062 G1). Only then are the record lines parsed, the checksum re-verified, and the batch
/// handed to the sink.
async fn push(
    State(server): State<Arc<SyncServer>>,
    Extension(node): Extension<AuthenticatedNode>,
    request: Request,
) -> std::result::Result<axum::Json<PushReply>, Failure> {
    let body = read_body(request, server.settings.max_push_bytes).await?;
    let header: BatchHeader = wire::decode_header(&body)?;
    if !node.owns(&header.source) {
        return Err(Error::partition_violation(node.node.as_str(), header.source.as_str()).into());
    }
    let records = wire::decode_records(&body)?;
    if records.len() > server.settings.max_batch_records {
        return Err(Error::batch_too_large(format!(
            "the batch carries {} records, above the {}-record limit",
            records.len(),
            server.settings.max_batch_records
        ))
        .into());
    }
    let batch = wire::batch_from_parts(&header, records)?;
    let reply = match server.sink.on_batch(&batch) {
        spot_sync_protocol::Outcome::Ack(ack) => PushReply::Ack(ack),
        spot_sync_protocol::Outcome::Nack(nack) => PushReply::Nack(nack),
        spot_sync_protocol::Outcome::Failed(error) => {
            return Err(
                Error::unavailable(format!("the apply path failed: {}", error.code)).into(),
            );
        }
        // `Outcome` is non-exhaustive: a future variant is treated as an internal failure.
        _ => {
            return Err(Error::unavailable("the apply path returned an unknown outcome").into());
        }
    };
    Ok(axum::Json(reply))
}

/// Reads a request body, mapping an oversize or unreadable body to a `sync_http.*` error.
async fn read_body(request: Request, limit: usize) -> std::result::Result<Bytes, Failure> {
    let bytes = axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|error| {
            Failure::from(Error::batch_too_large(format!(
                "the body could not be read within {limit} bytes: {error}"
            )))
        })?;
    Ok(bytes)
}

/// Parses and validates the `source` query parameter.
fn parse_source(params: &BTreeMap<String, String>) -> std::result::Result<SourceId, Failure> {
    let raw = params
        .get("source")
        .ok_or_else(|| Failure::from(Error::invalid_request("`source` is required")))?;
    SourceId::new(raw)
        .map_err(|error| Failure::from(Error::invalid_request(format!("bad `source`: {error}"))))
}

/// Parses a required unsigned query parameter.
fn parse_u64(params: &BTreeMap<String, String>, name: &str) -> std::result::Result<u64, Failure> {
    let raw = params
        .get(name)
        .ok_or_else(|| Failure::from(Error::invalid_request(format!("`{name}` is required"))))?;
    raw.parse::<u64>()
        .map_err(|_| Failure::from(Error::invalid_request(format!("`{name}` is not a number"))))
}

/// Parses an unsigned query parameter from its text.
fn parse_usize(value: &str, name: &str) -> std::result::Result<usize, Failure> {
    value
        .parse::<usize>()
        .map_err(|_| Failure::from(Error::invalid_request(format!("`{name}` is not a number"))))
}

/// The pull query for `source`, `from_seq` and `max`, as a client builds it.
pub fn pull_query(source: &SourceId, from_seq: u64, max: usize) -> String {
    format!(
        "source={}&from_seq={from_seq}&max={max}",
        percent_encode(source.as_str())
    )
}

/// The pull route's path.
pub const PULL_PATH: &str = "/sync/v1/pull";
/// The hello route's path.
pub const HELLO_PATH: &str = "/sync/v1/hello";
/// The push route's path.
pub const PUSH_PATH: &str = "/sync/v1/push";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::identity::StaticDirectory;

    fn source() -> SourceId {
        SourceId::new("edge-khi#branch_01").unwrap()
    }

    #[test]
    fn query_parsing_is_checked() {
        let mut params = BTreeMap::new();
        params.insert("source".to_owned(), "edge-khi#branch_01".to_owned());
        params.insert("from_seq".to_owned(), "3".to_owned());
        assert_eq!(parse_source(&params).unwrap(), source());
        assert_eq!(parse_u64(&params, "from_seq").unwrap(), 3);
        assert!(parse_source(&BTreeMap::new()).is_err());
        assert!(parse_u64(&params, "missing").is_err());
    }

    #[test]
    fn the_pull_query_encodes_the_source() {
        assert_eq!(
            pull_query(&source(), 4, 16),
            "source=edge-khi%23branch_01&from_seq=4&max=16"
        );
    }

    #[test]
    fn a_server_builds_a_router_without_panicking() {
        let directory = Arc::new(StaticDirectory::new());
        let hub = Arc::new(crate::hub::MemoryHub::new());
        let receiver = spot_sync_protocol::Receiver::new(
            spot_sync_protocol::ReceiverConfig::default(),
            spot_sync_protocol::MemorySink::new(),
            spot_sync_protocol::MemoryCheckpoints::new(),
        )
        .unwrap();
        let sink = Arc::new(crate::hub::SharedReceiver::new(receiver));
        let clock = Arc::new(spot_core::SystemClock::new());
        let server = SyncServer::new(
            NodeId::new("head-office").unwrap(),
            directory,
            hub,
            sink,
            clock,
        );
        let _router = server.router();
    }
}
