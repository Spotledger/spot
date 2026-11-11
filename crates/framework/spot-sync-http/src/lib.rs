//! The sync transport binding: the T1 state machines on an edge-initiated HTTP wire.
//!
//! ADR-062 section 3 fixes the topology: an edge always dials out to its hub, over TLS with node
//! identity, so the hub never opens a connection to an edge and the edge works behind NAT. This
//! crate is that binding. It adds no policy and no apply logic: it carries batches between the
//! [`Sender`](spot_sync_protocol::Sender) and [`Receiver`](spot_sync_protocol::Receiver) state
//! machines of WP-EDG-02.T1 and proves, in the tests, that the same guarantees hold on the wire.
//!
//! # The three endpoints
//!
//! * `POST /sync/v1/hello` — the handshake: protocol-version negotiation and the metadata epoch.
//! * `GET  /sync/v1/pull`  — a long-poll for the next batch, with heartbeats and a server-side cap.
//! * `POST /sync/v1/push`  — a streamed batch the hub applies.
//!
//! # Identity
//!
//! The hub authenticates the edge from its client certificate. The TLS layer puts the peer
//! certificate into the request as a [`ClientCertificate`](identity::ClientCertificate); the
//! [`NodeDirectory`](identity::NodeDirectory) maps it to an
//! [`AuthenticatedNode`](identity::AuthenticatedNode), deciding pinning and revocation. A request
//! without an enrolled certificate never reaches a handler. On a push, the receiver refuses a
//! batch whose source partition the authenticated node does not own
//! (`sync_http.partition_violation`) before it decodes any record beyond the header (ADR-062 G1).
//!
//! # Framing
//!
//! A batch streams as newline-delimited JSON: a header line, then one line per record
//! ([`wire`]). The header-first framing is what makes the partition check cheap and early.
//!
//! # Time and randomness are arguments
//!
//! Nothing here reads the wall clock or an ambient random source. The server takes a
//! [`Clock`](spot_core::Clock); the client's deadline, retry policy, jitter and sleeper are all
//! injected ([`resilience`]), so a test or the simulator is deterministic.
//!
//! # The signed hub time on the heartbeat
//!
//! A node may take hub time **only** from a message the hub signed: `spot-offline-identity`
//! accepts a signed `HubTime` and refuses a bare clock reading, because anyone on the path who
//! could set the time a node believes could stretch every lease. The hub's pull long-poll already
//! writes a keep-alive line every few seconds, so that line is the cheapest carrier of a fresh
//! signed time. This crate only carries it:
//!
//! * the hub signs with its timestamp key and hands the signed message's JSON to a
//!   [`HubTimeSource`]; the transport asks that source, per heartbeat, for
//!   the node the directory authenticated, and writes
//!   `{"heartbeat":true,"hub_time":<that value>}` ([`wire::heartbeat_line_with`]);
//! * the node's client passes each payload to its [`HubTimeSink`], which
//!   feeds `SnapshotStore::accept`; the store verifies the signature against its pinned root and
//!   timestamp key, refuses a time not later than the newest it accepted, and only then moves the
//!   hub clock and the leases.
//!
//! The payload is **untrusted input** to the sink: this crate never signs, verifies, stores or
//! interprets it, does not depend on `spot-offline-identity`, and a payload never changes what a
//! pull returns — no sequence number, batching or [`PullStep`] depends on it. A heartbeat without
//! a payload is byte-for-byte today's `{"heartbeat":true}` line, so an older reader is unaffected,
//! and a payload above [`wire::MAX_HEARTBEAT_PAYLOAD_BYTES`] is not sent at all.
//!
//! # The loopback
//!
//! [`transport::LoopbackTransport`] calls the hub's router in process, with no socket, so the
//! integration tests exercise the real handlers, the real middleware and the real client. The
//! socket tests in `tests/socket.rs` run the same client over mutual TLS on `127.0.0.1` with a
//! self-signed CA generated in the test.

#![warn(missing_docs)]

pub mod client;
pub mod error;
pub mod hub;
pub mod identity;
pub mod resilience;
pub mod server;
pub mod tls;
pub mod transport;
pub mod wire;

pub use client::{ClientConfig, ClientIdentity, EdgeClient, HubTimeSink, PullStep, PushStep};
pub use error::{CODES, Error, ErrorBody, Failure, Result};
pub use hub::{HubSource, MemoryHub, PushSink, SharedReceiver};
pub use identity::{AuthenticatedNode, ClientCertificate, NodeDirectory, StaticDirectory};
pub use resilience::{
    BreakerConfig, CircuitBreaker, Deadline, JitterSource, NoJitter, NoopSleeper, Permit,
    RetryPolicy, SeededJitter, Sleeper, TokioSleeper,
};
pub use server::{HubTimeSource, ServerSettings, SyncServer};
pub use transport::{HttpRequest, HttpResponse, LoopbackTransport, SocketTransport, Transport};

use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future: the return of an object-safe async method on a trait object.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-http";
