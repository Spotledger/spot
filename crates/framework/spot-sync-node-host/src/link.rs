//! One edge-to-hub link: hello, pull and push over a [`Transport`], single attempt per call.
//!
//! The retry policy of `EdgeClient` is deliberately not used here: the loop owns the backoff (a
//! failed call ends the session, the loop waits and starts a new one), so there is exactly one
//! place where time is spent. What stays per call is the [`CircuitBreaker`] (one per hub
//! address, so a dead hub is not dialled once per step) and the request deadline. A retry of
//! the same request is always safe: a pull is a read, and a push is idempotent because the
//! hub's apply is keyed by `(name, version)` and its checkpoint never runs ahead of its data
//! (ADR-062 G4).

use std::sync::Arc;
use std::time::Duration;

use http::Method;
use serde::Deserialize;
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_sync_http::server::{HELLO_PATH, PULL_PATH, PUSH_PATH, pull_query};
use spot_sync_http::wire::{self, CONTENT_TYPE, HelloReply, PushReply};
use spot_sync_http::{
    BreakerConfig, CircuitBreaker, ClientIdentity, HttpRequest, HttpResponse, HubTimeSink,
    Transport,
};
use spot_sync_protocol::{Batch, Hello};

use crate::error::{Error, Result};

/// The link to one hub.
pub struct HubLink {
    transport: Arc<dyn Transport>,
    identity: ClientIdentity,
    clock: Arc<dyn Clock>,
    breaker: CircuitBreaker,
    hub_key: String,
    deadline: Duration,
    max_batch_records: usize,
    hub_time: Option<Arc<dyn HubTimeSink>>,
}

impl std::fmt::Debug for HubLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubLink")
            .field("identity", &self.identity)
            .field("hub_key", &self.hub_key)
            .finish_non_exhaustive()
    }
}

impl HubLink {
    /// A link. `hub_key` names the destination for the breaker (the hub address, never a secret).
    ///
    /// # Errors
    ///
    /// `sync_http.invalid_request` when the breaker configuration is invalid.
    pub fn new(
        transport: Arc<dyn Transport>,
        identity: ClientIdentity,
        clock: Arc<dyn Clock>,
        breaker: BreakerConfig,
        hub_key: impl Into<String>,
        deadline: Duration,
        max_batch_records: usize,
    ) -> Result<Self> {
        Ok(Self {
            transport,
            identity,
            clock,
            breaker: CircuitBreaker::new(breaker)?,
            hub_key: hub_key.into(),
            deadline,
            max_batch_records,
            hub_time: None,
        })
    }

    /// Sets the sink that receives the pre-signed hub times a pull's heartbeats carry. The
    /// payload is untrusted input to the sink, which verifies it.
    pub fn with_hub_time_sink(mut self, sink: Arc<dyn HubTimeSink>) -> Self {
        self.hub_time = Some(sink);
        self
    }

    /// The handshake: protocol negotiation and the metadata-epoch check.
    ///
    /// # Errors
    ///
    /// The wire's code (`sync_http.version_mismatch`, `sync_http.metadata_epoch`,
    /// `sync_http.unavailable`, `sync_http.circuit_open`, ...).
    pub async fn hello(&self) -> Result<HelloReply> {
        let hello = Hello::new(
            self.identity.node.clone(),
            self.identity.protocol_range,
            self.identity.metadata_epoch,
        );
        let body = serde_json::to_vec(&hello).map_err(|_| {
            Error::new(
                spot_sync_http::error::INVALID_REQUEST,
                "the hello did not encode",
            )
        })?;
        let request =
            HttpRequest::new(Method::POST, HELLO_PATH, body).with_content_type("application/json");
        let response = self.call(request).await?;
        if !response.is_success() {
            return Err(remote_error(&response));
        }
        serde_json::from_slice(&response.body).map_err(|_| {
            Error::new(
                spot_sync_http::error::INVALID_REQUEST,
                "the hello reply did not parse",
            )
        })
    }

    /// Pulls the next batch of `source` from `from_seq`, at most `max` records (capped at the
    /// link's own limit times 16), or `None` when the hub had nothing before its long-poll cap.
    /// Heartbeat payloads are offered to the hub-time sink.
    ///
    /// # Errors
    ///
    /// The wire's code.
    pub async fn pull(
        &self,
        source: &SourceId,
        from_seq: u64,
        max: usize,
    ) -> Result<Option<Batch>> {
        let max = max.clamp(1, self.max_batch_records.saturating_mul(16).max(1));
        let query = pull_query(source, from_seq, max);
        let request = HttpRequest::get(PULL_PATH).with_query(query);
        let response = self.call(request).await?;
        if !response.is_success() {
            return Err(remote_error(&response));
        }
        self.offer_hub_times(&response.body);
        if !wire::contains_batch(&response.body) {
            return Ok(None);
        }
        Ok(Some(wire::decode_batch(&response.body)?))
    }

    /// Pushes one batch.
    ///
    /// # Errors
    ///
    /// The wire's code.
    pub async fn push(&self, batch: &Batch) -> Result<PushReply> {
        let body = wire::encode_batch(batch)?;
        let request =
            HttpRequest::new(Method::POST, PUSH_PATH, body).with_content_type(CONTENT_TYPE);
        let response = self.call(request).await?;
        if !response.is_success() {
            return Err(remote_error(&response));
        }
        serde_json::from_slice(&response.body).map_err(|_| {
            Error::new(
                spot_sync_http::error::INVALID_REQUEST,
                "the push reply did not parse",
            )
        })
    }

    /// Offers every heartbeat payload of a pull response to the sink. A panicking sink is caught.
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
            let offered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.offer(payload);
            }));
            if offered.is_err() {
                tracing::warn!("a hub time sink panicked; the pull continues without it");
            }
        }
    }

    /// One request under the breaker and the deadline.
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse> {
        let now = self.clock.monotonic();
        let permit = self.breaker.acquire(&self.hub_key, now)?;
        let outcome = tokio::time::timeout(self.deadline, self.transport.request(request)).await;
        match outcome {
            Err(_) => {
                permit.record_failure(self.clock.monotonic());
                Err(Error::new(
                    spot_sync_http::error::DEADLINE_EXCEEDED,
                    "the request passed its deadline",
                ))
            }
            Ok(Err(error)) => {
                permit.record_failure(self.clock.monotonic());
                Err(error.into())
            }
            Ok(Ok(response)) if response.status >= 500 => {
                permit.record_failure(self.clock.monotonic());
                Ok(response)
            }
            Ok(Ok(response)) => {
                permit.record_success();
                Ok(response)
            }
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

/// Rebuilds an error from a hub error body, or reports an unreadable one as unavailable.
fn remote_error(response: &HttpResponse) -> Error {
    match serde_json::from_slice::<RemoteError>(&response.body) {
        Ok(remote) => {
            spot_sync_http::Error::from_code(&remote.error.code, remote.error.message).into()
        }
        Err(_) => Error::new(
            spot_sync_http::error::UNAVAILABLE,
            format!("the hub answered {}", response.status),
        ),
    }
}
