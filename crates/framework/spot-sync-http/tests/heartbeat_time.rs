//! The heartbeat as the carrier of a pre-signed hub time, over the real handlers and
//! the real client on the in-process loopback.
//!
//! The transport carries an opaque payload and nothing else: the hub asks its
//! [`HubTimeSource`] for the node the directory authenticated, the client offers each payload to
//! its [`HubTimeSink`], and no pull result depends on either. These tests prove that carrying
//! works, that a hub without a source writes today's line byte for byte, that an oversize payload
//! is never sent, and that a payload can never arrive from anywhere but a heartbeat line.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

mod support;

use std::sync::{Arc, Mutex};

use support::*;

use spot_core::SystemClock;
use spot_sync_http::server::{PULL_PATH, pull_query};
use spot_sync_http::{
    BoxFuture, ClientCertificate, EdgeClient, Error, HttpRequest, HttpResponse, HubTimeSink,
    HubTimeSource, LoopbackTransport, MemoryHub, NoJitter, NodeDirectory, NoopSleeper, PullStep,
    ServerSettings, SharedReceiver, StaticDirectory, SyncServer, Transport, wire,
};
use spot_sync_protocol::{NodeId, ProtocolRange, SourceId};

/// The payload the folder below carries: shaped like a signed message, and opaque here.
fn payload() -> serde_json::Value {
    serde_json::json!({
        "message": "hub_time",
        "tenant": "khi",
        "node": "edge-khi",
        "epoch": 3,
        "hub_ms": 1_700_000_000_000_u64,
        "signature": "c2lnbmF0dXJl",
    })
}

/// A hub time source that answers with a fixed payload and records who it was asked about.
#[derive(Default)]
struct RecordingSource {
    payload: Option<serde_json::Value>,
    asked: Mutex<Vec<String>>,
}

impl RecordingSource {
    /// A source that answers with `payload`.
    fn offering(payload: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            payload: Some(payload),
            asked: Mutex::new(Vec::new()),
        })
    }

    /// The node names the source was asked about, in order.
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl HubTimeSource for RecordingSource {
    fn current(&self, node: &str) -> Option<serde_json::Value> {
        self.asked.lock().unwrap().push(node.to_owned());
        self.payload.clone()
    }
}

/// A sink that keeps every payload it is offered.
#[derive(Default)]
struct RecordingSink {
    offered: Mutex<Vec<serde_json::Value>>,
}

impl RecordingSink {
    /// The payloads offered so far.
    fn offered(&self) -> Vec<serde_json::Value> {
        self.offered.lock().unwrap().clone()
    }
}

impl HubTimeSink for RecordingSink {
    fn offer(&self, payload: serde_json::Value) {
        self.offered.lock().unwrap().push(payload);
    }
}

/// A sink that panics on every offer: the pull must survive it.
struct PanickingSink;

impl HubTimeSink for PanickingSink {
    fn offer(&self, _payload: serde_json::Value) {
        panic!("this sink panics on purpose");
    }
}

/// A hub that may carry a signed hub time, wired exactly as a deployment wires it.
fn time_hub(
    directory: Arc<dyn NodeDirectory>,
    stream: Arc<MemoryHub>,
    hub_time: Option<Arc<dyn HubTimeSource>>,
    settings: ServerSettings,
) -> Hub {
    let node = NodeId::new("head-office").unwrap();
    let sink = Arc::new(SharedReceiver::new(receiver()));
    let mut server = SyncServer::new(
        node.clone(),
        directory,
        stream,
        sink.clone(),
        Arc::new(SystemClock::new()),
    )
    .with_settings(settings)
    .with_identity(ProtocolRange::current(), 7);
    if let Some(hub_time) = hub_time {
        server = server.with_hub_time_source(hub_time);
    }
    Hub {
        router: server.router(),
        sink,
        node,
    }
}

/// A client that offers heartbeat payloads to `sink`.
fn time_client(
    hub: &Hub,
    node: &NodeId,
    certificate: ClientCertificate,
    sink: Option<Arc<dyn HubTimeSink>>,
) -> EdgeClient {
    let transport = Arc::new(LoopbackTransport::new(hub.router.clone(), certificate));
    let client = EdgeClient::new(
        transport,
        identity(node),
        client_config(64),
        Arc::new(SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(NoopSleeper),
        format!("hub:{}", hub.node),
    )
    .unwrap();
    match sink {
        Some(sink) => client.with_hub_time_sink(sink),
        None => client,
    }
}

/// A raw pull as `certificate`, returning the response body as text.
async fn pull_text(
    directory: Arc<dyn NodeDirectory>,
    stream: Arc<MemoryHub>,
    hub_time: Option<Arc<dyn HubTimeSource>>,
    certificate: &ClientCertificate,
    query: &str,
) -> String {
    let hub = time_hub(directory, stream, hub_time, settings());
    let transport = LoopbackTransport::new(hub.router, certificate.clone());
    let response = transport
        .request(HttpRequest::get(PULL_PATH).with_query(query))
        .await
        .unwrap();
    assert_eq!(response.status, 200, "the pull answered");
    response.text().unwrap().to_owned()
}

/// (a) A waiting pull's heartbeats carry the payload and the sink receives it.
#[tokio::test]
async fn a_waiting_pull_carries_the_payload_to_the_sink() {
    let source = SourceId::new("head-office#hub").unwrap();
    let certificate = certificate(21);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let payload = payload();
    let hub = time_hub(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        Arc::new(MemoryHub::new()),
        Some(RecordingSource::offering(payload.clone())),
        settings(),
    );

    let sink = Arc::new(RecordingSink::default());
    let client = time_client(&hub, &node, certificate, Some(sink.clone()));
    let mut receiver = receiver();
    let step = client.pull_once(&mut receiver, &source).await.unwrap();
    assert!(matches!(step, PullStep::Idle), "nothing was available");

    let offered = sink.offered();
    assert!(
        !offered.is_empty(),
        "the heartbeats of the waiting pull carried the payload"
    );
    assert!(
        offered.iter().all(|seen| seen == &payload),
        "every payload offered is the hub's: {offered:?}"
    );
    assert_eq!(receiver.sink().len(), 0, "no record was invented");
}

/// (b) The source is asked for the authenticated caller's node name — not one the caller can
/// put in the request — proven with two nodes on one hub.
#[tokio::test]
async fn the_source_is_asked_for_the_authenticated_caller() {
    let branch_01 = NodeId::new("edge-khi#branch_01").unwrap();
    let branch_02 = NodeId::new("edge-khi#branch_02").unwrap();
    let first = certificate(22);
    let second = certificate(23);
    let mut directory = StaticDirectory::new();
    directory.enroll(
        first.fingerprint(),
        branch_01.clone(),
        ["branch_01".to_owned()],
    );
    directory.enroll(
        second.fingerprint(),
        branch_02.clone(),
        ["branch_02".to_owned()],
    );
    let hub_source = RecordingSource::offering(payload());
    let directory: Arc<dyn NodeDirectory> = Arc::new(directory);
    let hub = time_hub(
        Arc::clone(&directory),
        Arc::new(MemoryHub::new()),
        Some(Arc::clone(&hub_source) as Arc<dyn HubTimeSource>),
        settings(),
    );
    let source = SourceId::new("head-office#hub").unwrap();
    // A node name in the query is not identity: the directory's answer is the only name the
    // source may see.
    let spoofed = format!("{}&node=edge-khi%23branch_02", pull_query(&source, 1, 8));

    for (certificate, expected) in [
        (&first, "edge-khi#branch_01"),
        (&second, "edge-khi#branch_02"),
    ] {
        hub_source.asked.lock().unwrap().clear();
        let transport = LoopbackTransport::new(hub.router.clone(), certificate.clone());
        let response = transport
            .request(HttpRequest::get(PULL_PATH).with_query(spoofed.clone()))
            .await
            .unwrap();
        assert!(response.text().unwrap().contains("\"heartbeat\":true"));
        let asked = hub_source.asked();
        assert!(!asked.is_empty(), "{expected} pulled and was heartbeated");
        assert!(
            asked.iter().all(|node| node == expected),
            "the source saw {asked:?}, expected only {expected}"
        );
    }
}

/// (c) With no source, the heartbeat line is byte-identical to `{"heartbeat":true}` plus a
/// newline: an older reader sees exactly today's wire.
#[tokio::test]
async fn without_a_source_the_heartbeat_is_unchanged() {
    let source = SourceId::new("head-office#hub").unwrap();
    let certificate = certificate(24);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let body = pull_text(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        Arc::new(MemoryHub::new()),
        None,
        &certificate,
        &pull_query(&source, 1, 8),
    )
    .await;

    assert!(body.ends_with('\n'), "every line is newline-terminated");
    let lines: Vec<&str> = body.split('\n').filter(|line| !line.is_empty()).collect();
    assert!(!lines.is_empty(), "an idle pull heartbeats");
    for line in &lines {
        assert_eq!(*line, "{\"heartbeat\":true}", "the line is today's line");
    }
    let mut expected = Vec::new();
    for _ in 0..lines.len() {
        expected.extend_from_slice(&wire::heartbeat_line());
    }
    assert_eq!(body.as_bytes(), expected.as_slice());
}

/// (d) The existing `is_heartbeat` recognises both forms; `heartbeat_payload` separates them.
#[test]
fn the_old_reader_still_recognises_both_forms() {
    let carrying = String::from_utf8(wire::heartbeat_line_with(Some(&payload())).to_vec()).unwrap();
    for line in [
        "{\"heartbeat\":true}",
        "{\"heartbeat\":true,\"hub_time\":null}",
        carrying.trim_end(),
    ] {
        assert!(wire::is_heartbeat(line), "{line} is a heartbeat");
    }
    assert!(!wire::is_heartbeat("{\"heartbeat\":false}"));
    assert_eq!(wire::heartbeat_payload("{\"heartbeat\":true}"), None);
    assert_eq!(
        wire::heartbeat_payload(carrying.trim_end()),
        Some(payload())
    );
    // A heartbeat with an unknown extra field is still skipped by the batch readers.
    assert!(!wire::contains_batch(
        b"{\"heartbeat\":true,\"hub_time\":\"x\"}\n"
    ));
}

/// (e) A payload above the limit is not sent, and the pull works exactly as before.
#[tokio::test]
async fn an_oversize_payload_is_not_sent_and_the_pull_still_works() {
    let source = SourceId::new("head-office#hub").unwrap();
    let certificate = certificate(25);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let stream = Arc::new(MemoryHub::new());
    let oversize = serde_json::json!("x".repeat(wire::MAX_HEARTBEAT_PAYLOAD_BYTES + 64));
    let hub = time_hub(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        Arc::clone(&stream),
        Some(RecordingSource::offering(oversize.clone())),
        settings(),
    );

    let sink = Arc::new(RecordingSink::default());
    let client = time_client(&hub, &node, certificate, Some(sink.clone()));
    let mut receiver = receiver();

    // The waiting pull heartbeats without the payload, and the sink sees nothing.
    let idle = client.pull_once(&mut receiver, &source).await.unwrap();
    assert!(matches!(idle, PullStep::Idle));
    assert!(
        sink.offered().is_empty(),
        "an oversize payload is never carried"
    );

    // The same hub still serves its batch: the refused payload changed nothing.
    stream.append(&source, records(&source, 1, 3));
    let step = client.pull_once(&mut receiver, &source).await.unwrap();
    assert!(matches!(step, PullStep::Applied(ack) if ack.through_seq == 3));
    assert_eq!(receiver.sink().len(), 3);
    assert!(sink.offered().is_empty());

    // A reader ignores an oversize payload even if a sender produced one anyway.
    let line = format!(
        "{{\"heartbeat\":true,\"hub_time\":{}}}",
        serde_json::to_string(&oversize).unwrap()
    );
    assert_eq!(wire::heartbeat_payload(&line), None);
    assert!(wire::is_heartbeat(&line), "it is still a heartbeat");
}

/// (f) A sink that panics does not fail the pull.
#[tokio::test]
async fn a_panicking_sink_does_not_fail_the_pull() {
    let source = SourceId::new("head-office#hub").unwrap();
    let certificate = certificate(26);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let stream = Arc::new(MemoryHub::new());
    let hub = time_hub(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        Arc::clone(&stream),
        Some(RecordingSource::offering(payload())),
        settings(),
    );
    let client = time_client(
        &hub,
        &node,
        certificate,
        Some(Arc::new(PanickingSink) as Arc<dyn HubTimeSink>),
    );
    let mut receiver = receiver();

    let idle = client.pull_once(&mut receiver, &source).await;
    assert!(
        matches!(idle, Ok(PullStep::Idle)),
        "the panic was contained: {idle:?}"
    );

    stream.append(&source, records(&source, 1, 2));
    let step = client.pull_once(&mut receiver, &source).await.unwrap();
    assert!(matches!(step, PullStep::Applied(ack) if ack.through_seq == 2));
    assert_eq!(receiver.sink().len(), 2);
}

/// (g) A `hub_time` field on a line that is not a heartbeat is never offered.
#[tokio::test]
async fn a_payload_is_never_offered_from_a_non_heartbeat_line() {
    let source = SourceId::new("head-office#hub").unwrap();
    let certificate = certificate(27);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let stream = Arc::new(MemoryHub::new());
    // A batch is already available, so the response opens with the batch header, not a heartbeat.
    stream.append(&source, records(&source, 1, 2));
    let hub = time_hub(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        Arc::clone(&stream),
        None,
        settings(),
    );

    let inner: Arc<dyn Transport> = Arc::new(LoopbackTransport::new(
        hub.router.clone(),
        certificate.clone(),
    ));
    let transport: Arc<dyn Transport> = Arc::new(TamperingTransport { inner });
    let sink = Arc::new(RecordingSink::default());
    let client = EdgeClient::new(
        transport,
        identity(&node),
        client_config(64),
        Arc::new(SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(NoopSleeper),
        format!("hub:{}", hub.node),
    )
    .unwrap()
    .with_hub_time_sink(sink.clone());

    let mut receiver = receiver();
    let step = client.pull_once(&mut receiver, &source).await.unwrap();
    assert!(
        matches!(step, PullStep::Applied(ack) if ack.through_seq == 2),
        "the forged field changed nothing about the batch"
    );
    assert_eq!(receiver.sink().len(), 2, "the batch still applied");
    assert!(
        sink.offered().is_empty(),
        "a non-heartbeat line's `hub_time` was offered: {:?}",
        sink.offered()
    );

    // The line the transport forged, read on its own: not a heartbeat, so no payload.
    let forged = "{\"hub_time\":\"forged\",\"source\":\"head-office#hub\",\"from_seq\":1,\
                  \"to_seq\":2,\"checksum\":\"x\"}";
    assert_eq!(wire::heartbeat_payload(forged), None);
    assert!(!wire::is_heartbeat(forged));
}

/// Adds a `hub_time` field to the first line of every pull response: a header or a record line
/// that is not a heartbeat.
struct TamperingTransport {
    inner: Arc<dyn Transport>,
}

impl Transport for TamperingTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse, Error>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let response = inner.request(request).await?;
            let text = response.text()?;
            let forged = text.replacen('{', "{\"hub_time\":\"forged\",", 1);
            Ok(HttpResponse {
                status: response.status,
                body: forged.into(),
            })
        })
    }
}
