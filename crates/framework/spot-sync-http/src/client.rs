//! The edge side: an [`EdgeClient`] that drives the sender and receiver state machines over a
//! [`Transport`].
//!
//! The client owns the outbound `Sender` (for push) and drives a caller-owned `Receiver` (for
//! pull), so the state machines stay where WP-EDG-02.T1 put them and this crate only adds the
//! wire, the identity and the resilience:
//!
//! * every call runs under a [`Deadline`] and through a [`CircuitBreaker`], with bounded retries
//!   and injected jitter (ADR-072);
//! * a retry re-sends the same request, which is always safe: a pull is a read, and a push is
//!   idempotent because the receiver's durable apply is keyed by `(name, version)` and its
//!   checkpoint never runs ahead of the data (ADR-062 G4). The retry never advances a cursor
//!   without the checkpoint check the receiver makes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::Method;
use hyper::body::Bytes;
use serde::Deserialize;
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_sync_protocol::{
    Ack, Action, ApplySink, CheckpointStore, Hello, Nack, NodeId, Offer, Outcome, ProtocolRange,
    Receiver, Sender, SenderConfig,
};

use crate::error::{Error, Result};
use crate::resilience::{
    BreakerConfig, CircuitBreaker, Deadline, JitterSource, RetryPolicy, Sleeper,
};
use crate::server::{HELLO_PATH, PULL_PATH, PUSH_PATH, pull_query};
use crate::transport::{HttpRequest, HttpResponse, Transport};
use crate::wire::{self, CONTENT_TYPE, HelloReply, PushReply};

/// Who the edge is: its node identity and the handshake fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    /// The edge's node id.
    pub node: NodeId,
    /// The protocol versions the edge speaks.
    pub protocol_range: ProtocolRange,
    /// The metadata epoch the edge's records were written under.
    pub metadata_epoch: u64,
}

impl ClientIdentity {
    /// An identity that speaks only the current protocol version at epoch zero.
    pub fn new(node: NodeId) -> Self {
        Self {
            node,
            protocol_range: ProtocolRange::current(),
            metadata_epoch: 0,
        }
    }

    /// Sets the protocol range and metadata epoch.
    pub fn with_protocol(mut self, protocol_range: ProtocolRange, metadata_epoch: u64) -> Self {
        self.protocol_range = protocol_range;
        self.metadata_epoch = metadata_epoch;
        self
    }
}

/// The edge's resilience and flow-control settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// The total budget for one session step, across attempts.
    pub deadline: Duration,
    /// The bounded retry policy.
    pub retry: RetryPolicy,
    /// The breaker for the hub destination.
    pub breaker: BreakerConfig,
    /// The most records one pull or push batch may carry.
    pub max_batch_records: usize,
}

impl ClientConfig {
    /// A configuration; fails on a zero deadline or batch size.
    pub fn new(
        deadline: Duration,
        retry: RetryPolicy,
        breaker: BreakerConfig,
        max_batch_records: usize,
    ) -> Result<Self> {
        if deadline.is_zero() {
            return Err(Error::invalid_request(
                "a session deadline is at least one nanosecond",
            ));
        }
        if max_batch_records == 0 {
            return Err(Error::invalid_request(
                "a batch carries at least one record",
            ));
        }
        // Re-validate through the constructors so a struct literal cannot bypass them.
        let retry = RetryPolicy::new(retry.max_attempts, retry.base, retry.factor, retry.cap)
            .map_err(|error| Error::invalid_request(error.to_string()))?;
        let breaker = BreakerConfig::new(
            breaker.failure_threshold,
            breaker.open_for,
            breaker.half_open_probes,
        )?;
        Ok(Self {
            deadline,
            retry,
            breaker,
            max_batch_records,
        })
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(30),
            retry: RetryPolicy::new(3, Duration::from_millis(100), 2, Duration::from_secs(5))
                .unwrap_or(RetryPolicy {
                    max_attempts: 1,
                    base: Duration::from_millis(100),
                    factor: 2,
                    cap: Duration::from_secs(5),
                }),
            breaker: BreakerConfig::default(),
            max_batch_records: 1_024,
        }
    }
}

/// What one pull step did.
#[derive(Debug)]
pub enum PullStep {
    /// Nothing was available; the hub's long-poll reached its cap.
    Idle,
    /// A batch was applied durably and acknowledged.
    Applied(Ack),
    /// A batch was refused; the hub will re-serve from the receiver's checkpoint.
    Nacked(Nack),
}

/// What one push step did.
#[derive(Debug)]
pub enum PushStep {
    /// Nothing was offered, or the in-flight window is full.
    Idle,
    /// The hub acknowledged the batch.
    Acked(Ack),
    /// The hub refused the batch; the sender rewound and will resend.
    Nacked(Nack),
}

/// Receives the pre-signed hub time a pull's heartbeats carry.
///
/// The payload is **untrusted input**: it is whatever the hub put on the wire, this crate neither
/// verifies nor keeps it, and anybody on the path could have changed it. Only the node's snapshot
/// store (`spot-offline-identity`'s `SnapshotStore::accept`) may turn it into a hub time, by
/// verifying the signature against its pinned root and timestamp key and refusing anything older
/// than the newest time it accepted. A sink is free to reject a payload; it must not
/// trust one.
pub trait HubTimeSink: Send + Sync {
    /// Offers one heartbeat payload. Called on the pull's path, several times per pull; must be
    /// cheap and must not block for long. A panic here is caught and does not fail the pull.
    fn offer(&self, payload: serde_json::Value);
}

/// The edge's client: a transport, an identity and the two state machines.
pub struct EdgeClient {
    transport: Arc<dyn Transport>,
    identity: ClientIdentity,
    config: ClientConfig,
    clock: Arc<dyn Clock>,
    jitter: Mutex<Box<dyn JitterSource>>,
    breaker: CircuitBreaker,
    sleeper: Arc<dyn Sleeper>,
    hub_key: String,
    sender: Sender,
    hub_time: Option<Arc<dyn HubTimeSink>>,
}

impl std::fmt::Debug for EdgeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeClient")
            .field("identity", &self.identity)
            .field("config", &self.config)
            .field("hub_key", &self.hub_key)
            .finish_non_exhaustive()
    }
}

impl EdgeClient {
    /// Assembles a client.
    ///
    /// `hub_key` names the destination for the breaker (for example `hub:head-office`).
    pub fn new(
        transport: Arc<dyn Transport>,
        identity: ClientIdentity,
        config: ClientConfig,
        clock: Arc<dyn Clock>,
        jitter: Box<dyn JitterSource>,
        sleeper: Arc<dyn Sleeper>,
        hub_key: impl Into<String>,
    ) -> Result<Self> {
        let config = ClientConfig::new(
            config.deadline,
            config.retry,
            config.breaker,
            config.max_batch_records,
        )?;
        let breaker = CircuitBreaker::new(config.breaker)?;
        let sender = Sender::new(
            SenderConfig::new(config.max_batch_records, 4_096)
                .map_err(|error| Error::invalid_request(error.to_string()))?,
        )
        .map_err(|error| Error::invalid_request(error.to_string()))?;
        Ok(Self {
            transport,
            identity,
            config,
            clock,
            jitter: Mutex::new(jitter),
            breaker,
            sleeper,
            hub_key: hub_key.into(),
            sender,
            hub_time: None,
        })
    }

    /// Sets the sink the pre-signed hub times of a pull's heartbeats are offered to.
    ///
    /// None by default: a client with no sink reads heartbeats exactly as it always has and never
    /// looks at a payload. Offering never changes what a pull returns — no sequence number, no
    /// batch and no [`PullStep`] depends on it.
    pub fn with_hub_time_sink(mut self, sink: Arc<dyn HubTimeSink>) -> Self {
        self.hub_time = Some(sink);
        self
    }

    /// The client's configuration.
    pub fn config(&self) -> ClientConfig {
        self.config
    }

    /// The outbound sender, for a caller that drives push itself.
    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    /// The handshake: negotiate a version and check the metadata epoch.
    ///
    /// # Errors
    ///
    /// [`crate::error::VERSION_MISMATCH`] when the ranges do not overlap, and
    /// [`crate::error::METADATA_EPOCH`] when the epochs differ.
    pub async fn hello(&self) -> Result<HelloReply> {
        let hello = Hello::new(
            self.identity.node.clone(),
            self.identity.protocol_range,
            self.identity.metadata_epoch,
        );
        let body = serde_json::to_vec(&hello).map_err(|error| {
            Error::invalid_request(format!("could not encode the hello: {error}"))
        })?;
        let request = HttpRequest::new(Method::POST, HELLO_PATH, Bytes::from(body))
            .with_content_type("application/json");
        let response = self.call(request).await?;
        if !response.is_success() {
            return Err(remote_error(&response));
        }
        serde_json::from_slice(&response.body).map_err(|error| {
            Error::invalid_request(format!("the hello reply did not parse: {error}"))
        })
    }

    /// Pulls one batch and applies it to `receiver`.
    pub async fn pull_once<A: ApplySink, C: CheckpointStore>(
        &self,
        receiver: &mut Receiver<A, C>,
        source: &SourceId,
    ) -> Result<PullStep> {
        let from_seq = receiver
            .stored_checkpoint(source)
            .map_or(1, |checkpoint| checkpoint.next_expected());
        let query = pull_query(source, from_seq, self.config.max_batch_records);
        let request = HttpRequest::new(Method::GET, PULL_PATH, Bytes::new()).with_query(query);
        let response = self.call(request).await?;
        if !response.is_success() {
            return Err(remote_error(&response));
        }
        // The heartbeat payloads are offered before the body is read as a batch, and the reading
        // below is exactly as it was: a payload never changes what the pull returns.
        self.offer_hub_times(&response.body);
        if !wire::contains_batch(&response.body) {
            return Ok(PullStep::Idle);
        }
        let batch = wire::decode_batch(&response.body)?;
        match receiver.on_batch(&batch) {
            Outcome::Ack(ack) => Ok(PullStep::Applied(ack)),
            Outcome::Nack(nack) => Ok(PullStep::Nacked(nack)),
            Outcome::Failed(error) => Err(Error::unavailable(format!(
                "the apply path failed: {}",
                error.code
            ))),
            // `Outcome` is non-exhaustive: a future variant is an internal failure.
            _ => Err(Error::unavailable(
                "the apply path returned an unknown outcome",
            )),
        }
    }

    /// Pulls until the hub reports nothing, returning the last sequence applied.
    ///
    /// `max_steps` bounds the loop so a persistently refusing hub cannot spin forever.
    pub async fn pull_until_idle<A: ApplySink, C: CheckpointStore>(
        &self,
        receiver: &mut Receiver<A, C>,
        source: &SourceId,
        max_steps: usize,
    ) -> Result<u64> {
        let mut applied = receiver
            .stored_checkpoint(source)
            .map_or(0, |checkpoint| checkpoint.last_applied_seq);
        let mut steps = 0;
        while steps < max_steps {
            match self.pull_once(receiver, source).await? {
                PullStep::Idle => break,
                PullStep::Applied(ack) => applied = applied.max(ack.through_seq),
                PullStep::Nacked(_) => {}
            }
            steps += 1;
        }
        Ok(applied)
    }

    /// Offers the sender's next batch to the hub and applies the reply.
    pub async fn push_once(&mut self, offer: &Offer) -> Result<PushStep> {
        let Action::Send(batch) = self.sender.next(offer) else {
            return Ok(PushStep::Idle);
        };
        let body = wire::encode_batch(&batch)?;
        let request =
            HttpRequest::new(Method::POST, PUSH_PATH, body).with_content_type(CONTENT_TYPE);
        let response = self.call(request).await?;
        if !response.is_success() {
            return Err(remote_error(&response));
        }
        let reply: PushReply = serde_json::from_slice(&response.body).map_err(|error| {
            Error::invalid_request(format!("the push reply did not parse: {error}"))
        })?;
        match reply {
            PushReply::Ack(ack) => {
                self.sender.on_ack(&ack);
                Ok(PushStep::Acked(ack))
            }
            PushReply::Nack(nack) => {
                self.sender.on_nack(&nack);
                Ok(PushStep::Nacked(nack))
            }
        }
    }

    /// Pushes until the offer is exhausted, returning the acknowledged high-water mark.
    pub async fn push_until_idle(&mut self, offer: &Offer, max_steps: usize) -> Result<u64> {
        let source = offer.source.clone();
        let mut steps = 0;
        while steps < max_steps {
            match self.push_once(offer).await? {
                PushStep::Idle => break,
                PushStep::Acked(_) | PushStep::Nacked(_) => {}
            }
            steps += 1;
        }
        Ok(self.sender.acked_through(&source))
    }

    /// Offers every heartbeat payload in a pull response to the sink, if there is one.
    ///
    /// Only a heartbeat line can carry a payload: [`wire::heartbeat_payload`] answers `None` for
    /// a batch header or a record that happens to have a `hub_time` field. The payload is neither
    /// interpreted nor kept here — only its size is logged, at `trace`, and the sink decides what
    /// it means after verifying it. A sink that panics is caught so the pull still completes.
    fn offer_hub_times(&self, body: &[u8]) {
        let Some(sink) = &self.hub_time else {
            return;
        };
        let Ok(text) = std::str::from_utf8(body) else {
            return;
        };
        for line in text.split('\n').filter(|line| !line.trim().is_empty()) {
            let Some(payload) = wire::heartbeat_payload(line) else {
                continue;
            };
            tracing::trace!(
                bytes = serde_json::to_vec(&payload).map_or(0, |encoded| encoded.len()),
                "a heartbeat carried a hub time payload"
            );
            let offered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.offer(payload);
            }));
            if offered.is_err() {
                tracing::warn!("a hub time sink panicked; the pull continues without it");
            }
        }
    }

    /// One request under the deadline, the breaker and the retry policy.
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse> {
        let deadline = Deadline::new(self.clock.monotonic(), self.config.deadline);
        let mut attempt: u32 = 1;
        loop {
            let now = self.clock.monotonic();
            if deadline.is_expired(now) {
                return Err(Error::deadline_exceeded("the session deadline passed"));
            }
            let permit = self.breaker.acquire(&self.hub_key, now)?;
            let remaining = deadline.remaining(now);
            let outcome =
                tokio::time::timeout(remaining, self.transport.request(request.clone())).await;
            match outcome {
                Err(_) => {
                    permit.record_failure(now);
                    return Err(Error::deadline_exceeded(
                        "the request passed the session deadline",
                    ));
                }
                Ok(Ok(response)) if response.status < 500 => {
                    permit.record_success();
                    return Ok(response);
                }
                Ok(Ok(_)) | Ok(Err(_)) => {
                    permit.record_failure(now);
                }
            }
            if !self.config.retry.allows(attempt.saturating_add(1)) {
                return Err(Error::unavailable(format!(
                    "the hub was unavailable after {attempt} attempts"
                )));
            }
            let jitter = self
                .jitter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .permille();
            let delay = self.config.retry.delay(attempt.saturating_add(1), jitter);
            if deadline.remaining(self.clock.monotonic()) <= delay {
                return Err(Error::deadline_exceeded(
                    "the next retry would pass the session deadline",
                ));
            }
            self.sleeper.sleep(delay).await;
            attempt = attempt.saturating_add(1);
        }
    }
}

/// The `{ "error": { "code", "message" } }` body a hub sends on failure.
#[derive(Deserialize)]
struct RemoteError {
    error: RemoteErrorDetail,
}

#[derive(Deserialize)]
struct RemoteErrorDetail {
    code: String,
    message: String,
}

/// Rebuilds a client error from a hub error body, or reports an unreadable body.
fn remote_error(response: &HttpResponse) -> Error {
    match serde_json::from_slice::<RemoteError>(&response.body) {
        Ok(remote) => Error::from_code(&remote.error.code, remote.error.message),
        Err(_) => Error::unavailable(format!("the hub answered {}", response.status)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_config_validates_its_parts() {
        let retry =
            RetryPolicy::new(3, Duration::from_millis(10), 2, Duration::from_secs(1)).unwrap();
        assert!(ClientConfig::new(Duration::ZERO, retry, BreakerConfig::default(), 8).is_err());
        assert!(
            ClientConfig::new(Duration::from_secs(1), retry, BreakerConfig::default(), 0).is_err()
        );
        assert!(ClientConfig::default().max_batch_records > 0);
    }

    #[test]
    fn remote_errors_keep_known_codes_and_map_the_rest() {
        let response = HttpResponse {
            status: 403,
            body: Bytes::from_static(
                br#"{"error":{"code":"sync_http.partition_violation","message":"no","retryable":false,"request_id":""}}"#,
            ),
        };
        assert_eq!(
            remote_error(&response).code,
            crate::error::PARTITION_VIOLATION
        );
        let unknown = HttpResponse {
            status: 400,
            body: Bytes::from_static(
                br#"{"error":{"code":"sync_http.something_new","message":"x","retryable":false,"request_id":""}}"#,
            ),
        };
        assert_eq!(remote_error(&unknown).code, crate::error::UNAVAILABLE);
        let garbage = HttpResponse {
            status: 500,
            body: Bytes::from_static(b"boom"),
        };
        assert_eq!(remote_error(&garbage).code, crate::error::UNAVAILABLE);
    }
}
