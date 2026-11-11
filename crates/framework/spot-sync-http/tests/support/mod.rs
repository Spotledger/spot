//! Shared harness for the integration suites: an in-memory hub, a directory, a router and a
//! loopback client, all wired exactly as a deployment wires them.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]
// Each integration test compiles this module separately, so helpers another suite uses are dead
// code in this one.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use spot_core::SystemClock;
use spot_sync_http::{
    ClientConfig, ClientIdentity, EdgeClient, LoopbackTransport, MemoryHub, NoJitter,
    NodeDirectory, NoopSleeper, ServerSettings, SharedReceiver, StaticDirectory, SyncServer,
};
use spot_sync_protocol::{
    MemoryCheckpoints, MemorySink, NodeId, ProtocolRange, Receiver, Record, RecordName,
    RetryPolicy, SourceId,
};

/// The receiver the hub applies pushes to, in memory.
pub type TestReceiver = SharedReceiver<MemorySink, MemoryCheckpoints>;

/// A running in-process hub and the handles a test needs.
pub struct Hub {
    /// The router the loopback client calls.
    pub router: Router,
    /// The sink the push path applies to.
    pub sink: Arc<TestReceiver>,
    /// The hub node id.
    pub node: NodeId,
}

/// Builds a hub that serves `stream` and applies to `sink`, with `directory` resolving identities.
pub fn hub(
    directory: Arc<dyn NodeDirectory>,
    stream: Arc<MemoryHub>,
    sink: Arc<TestReceiver>,
    settings: ServerSettings,
) -> Hub {
    let node = NodeId::new("head-office").unwrap();
    let server = SyncServer::new(
        node.clone(),
        directory,
        stream,
        sink.clone(),
        Arc::new(SystemClock::new()),
    )
    .with_settings(settings)
    .with_identity(ProtocolRange::current(), 7);
    Hub {
        router: server.router(),
        sink,
        node,
    }
}

/// A directory that enrols `certificate` for `node`, owning `partitions`.
pub fn directory(
    certificate: &spot_sync_http::ClientCertificate,
    node: &NodeId,
    partitions: &[&str],
) -> StaticDirectory {
    let mut directory = StaticDirectory::new();
    directory.enroll(
        certificate.fingerprint(),
        node.clone(),
        partitions.iter().map(|p| (*p).to_owned()),
    );
    directory
}

/// A throwaway client certificate: a DER-shaped blob whose fingerprint is what the directory
/// pins. The loopback never parses it, so a real X.509 certificate is unnecessary.
pub fn certificate(seed: u8) -> spot_sync_http::ClientCertificate {
    spot_sync_http::ClientCertificate::from_der(vec![0x30, 0x82, seed, seed, seed, seed])
}

/// The default client config: a short deadline, a few retries, no jitter, no real sleeping.
pub fn client_config(max_batch_records: usize) -> ClientConfig {
    ClientConfig::new(
        Duration::from_secs(30),
        RetryPolicy::new(3, Duration::from_millis(1), 2, Duration::from_millis(10)).unwrap(),
        Default::default(),
        max_batch_records,
    )
    .unwrap()
}

/// A loopback client for `node`, presenting `certificate`, talking to `hub.router`.
pub fn client(
    hub: &Hub,
    node: &NodeId,
    certificate: spot_sync_http::ClientCertificate,
) -> EdgeClient {
    client_with(hub, node, certificate, client_config(64))
}

/// The client identity that matches the hub this harness builds (epoch 7).
pub fn identity(node: &NodeId) -> ClientIdentity {
    ClientIdentity::new(node.clone()).with_protocol(ProtocolRange::current(), 7)
}

/// A loopback client with an explicit config.
pub fn client_with(
    hub: &Hub,
    node: &NodeId,
    certificate: spot_sync_http::ClientCertificate,
    config: ClientConfig,
) -> EdgeClient {
    let transport = Arc::new(LoopbackTransport::new(hub.router.clone(), certificate));
    EdgeClient::new(
        transport,
        identity(node),
        config,
        Arc::new(SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(NoopSleeper),
        format!("hub:{}", hub.node),
    )
    .unwrap()
}

/// A fresh in-memory receiver.
pub fn receiver() -> Receiver<MemorySink, MemoryCheckpoints> {
    Receiver::new(
        spot_sync_protocol::ReceiverConfig::default(),
        MemorySink::new(),
        MemoryCheckpoints::new(),
    )
    .unwrap()
}

/// `count` records for `source`, at sequences `from..from+count`, version 1.
pub fn records(source: &SourceId, from: u64, count: u64) -> Vec<Record> {
    (0..count)
        .map(|index| {
            let seq = from + index;
            Record::new(
                RecordName::new(&format!("{}:doc_{seq}", source.site())).unwrap(),
                1,
                seq,
                format!("v{seq}"),
            )
            .unwrap()
        })
        .collect()
}

/// The default hub settings: a short long-poll and heartbeat so tests finish quickly.
pub fn settings() -> ServerSettings {
    ServerSettings {
        max_batch_records: 64,
        max_push_bytes: 1 << 20,
        long_poll: Duration::from_millis(60),
        heartbeat: Duration::from_millis(10),
    }
}
