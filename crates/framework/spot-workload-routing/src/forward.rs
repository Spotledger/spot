//! Forwarding: the loop-prevention header and deadline propagation (ADR-24).
//!
//! When a path rule sends a request to another node, the request carries two extra headers:
//!
//! - `x-spot-routing-hops` — how many times it has already been forwarded. The router refuses to
//!   forward a request whose hop count has reached the policy's `max_hops`, so a misconfigured
//!   rule can never make two nodes bounce a request forever.
//! - `x-spot-deadline-ms` — the remaining end-to-end budget in milliseconds. The client copies it
//!   onto the forwarded request, so the target node starts with what is left, not a fresh budget.
//!
//! [`ForwardingClient`] wraps a [`Transport`] (the thing that actually speaks HTTP) and applies
//! both headers. The transport is injected, so this crate never depends on a particular HTTP
//! client and a test uses [`RecordingTransport`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use http::{HeaderName, HeaderValue, Request, Response};

use crate::error::{Error, Result};
use crate::node::NodeId;

/// The hop-count header. Lowercase: `HeaderName::from_static` requires it.
pub const HOP_HEADER: &str = "x-spot-routing-hops";
/// The deadline header, in milliseconds remaining.
pub const DEADLINE_HEADER: &str = "x-spot-deadline-ms";
/// The header naming the node a request was forwarded to (diagnostics only; never trusted for a
/// decision).
pub const TARGET_HEADER: &str = "x-spot-routing-target";

/// A boxed, sendable future — the return of an async trait method without `async fn` in traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The remaining end-to-end budget for a forwarded request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    /// What is left of the budget.
    pub remaining: Duration,
}

impl Deadline {
    /// A deadline with `remaining` left.
    #[must_use]
    pub fn from_remaining(remaining: Duration) -> Self {
        Self { remaining }
    }

    /// A deadline from whole milliseconds.
    #[must_use]
    pub fn from_millis(millis: u64) -> Self {
        Self {
            remaining: Duration::from_millis(millis),
        }
    }

    /// The remaining budget in whole milliseconds.
    #[must_use]
    pub fn as_millis(self) -> u64 {
        u64::try_from(self.remaining.as_millis()).unwrap_or(u64::MAX)
    }

    /// The header value a forwarded request carries.
    #[must_use]
    pub fn to_header_value(self) -> HeaderValue {
        // A decimal millisecond count is always a valid header value; fall back to zero.
        HeaderValue::from_str(&self.as_millis().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0"))
    }

    /// Read a deadline from a header value, when present and well formed.
    #[must_use]
    pub fn from_header_value(value: Option<&HeaderValue>) -> Option<Self> {
        let millis = value?.to_str().ok()?.parse::<u64>().ok()?;
        Some(Self::from_millis(millis))
    }
}

/// What a forwarded request carries: how many hops it has taken and its remaining budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForwardMeta {
    /// How many times the request has already been forwarded.
    pub hops: u32,
    /// The remaining end-to-end budget, when one was set.
    pub deadline: Option<Deadline>,
}

impl ForwardMeta {
    /// The meta of a request that has not been forwarded yet.
    #[must_use]
    pub fn initial(deadline: Option<Deadline>) -> Self {
        Self { hops: 0, deadline }
    }

    /// The meta of the next hop, refusing when the hop budget is spent.
    ///
    /// # Errors
    ///
    /// [`Error::HopLimit`] when `self.hops` has reached `max_hops`.
    pub fn next(&self, target: &NodeId, max_hops: u32) -> Result<Self> {
        if self.hops >= max_hops {
            return Err(Error::HopLimit {
                target: target.to_string(),
                hops: self.hops,
                max_hops,
            });
        }
        Ok(Self {
            hops: self.hops.saturating_add(1),
            deadline: self.deadline,
        })
    }

    /// Read the hop count from a request's headers; an absent or malformed header is zero hops.
    #[must_use]
    pub fn hops_from_headers(headers: &http::HeaderMap) -> u32 {
        headers
            .get(HeaderName::from_static(HOP_HEADER))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0)
    }

    /// Read the deadline from a request's headers.
    #[must_use]
    pub fn deadline_from_headers(headers: &http::HeaderMap) -> Option<Deadline> {
        Deadline::from_header_value(headers.get(HeaderName::from_static(DEADLINE_HEADER)))
    }

    /// Stamp this meta onto an outgoing request: the next hop count and the deadline.
    pub fn apply<B>(&self, request: &mut Request<B>) {
        let headers = request.headers_mut();
        headers.insert(
            HeaderName::from_static(HOP_HEADER),
            HeaderValue::from_str(&self.hops.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
        if let Some(deadline) = self.deadline {
            headers.insert(
                HeaderName::from_static(DEADLINE_HEADER),
                deadline.to_header_value(),
            );
        }
    }
}

/// The thing that actually delivers a forwarded request. Implemented by the host's HTTP client;
/// [`RecordingTransport`] is provided for tests and for wiring before a real client lands.
pub trait Transport {
    /// The body type the transport moves.
    type Body;

    /// Deliver `request` to `target`.
    fn send(
        &self,
        target: NodeId,
        request: Request<Self::Body>,
    ) -> BoxFuture<'_, Result<Response<Self::Body>>>;
}

/// The forwarding client: applies the loop-prevention header and deadline propagation over a
/// [`Transport`].
#[derive(Clone, Debug)]
pub struct ForwardingClient<T> {
    transport: T,
    max_hops: u32,
}

impl<T> ForwardingClient<T> {
    /// A client over a transport with a hop budget.
    #[must_use]
    pub fn new(transport: T, max_hops: u32) -> Self {
        Self {
            transport,
            max_hops,
        }
    }

    /// The hop budget.
    #[must_use]
    pub fn max_hops(&self) -> u32 {
        self.max_hops
    }

    /// The wrapped transport.
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: Transport + Send + Sync> ForwardingClient<T>
where
    T::Body: Send,
{
    /// Forward a request to a target node, carrying the next hop count and the deadline. Returns
    /// [`Error::HopLimit`] without touching the transport when the hop budget is spent.
    pub fn forward<'a>(
        &'a self,
        target: NodeId,
        mut request: Request<T::Body>,
        meta: ForwardMeta,
    ) -> BoxFuture<'a, Result<Response<T::Body>>> {
        Box::pin(async move {
            let next = meta.next(&target, self.max_hops)?;
            next.apply(&mut request);
            request.headers_mut().insert(
                HeaderName::from_static(TARGET_HEADER),
                HeaderValue::from_str(target.as_str())
                    .unwrap_or_else(|_| HeaderValue::from_static("-")),
            );
            self.transport.send(target, request).await
        })
    }
}

/// One recorded forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedForward {
    /// The node the request went to.
    pub target: NodeId,
    /// The hop count stamped on the forwarded request.
    pub hops: u32,
    /// The deadline stamped on the forwarded request, in milliseconds.
    pub deadline_ms: Option<u64>,
}

/// An in-memory [`Transport`] that records what it was asked to forward and answers with a fixed
/// status. The host replaces it with a real client; tests assert against its record.
#[derive(Debug, Default)]
pub struct RecordingTransport {
    sent: Mutex<Vec<RecordedForward>>,
}

impl RecordingTransport {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything forwarded so far, in order.
    #[must_use]
    pub fn forwarded(&self) -> Vec<RecordedForward> {
        match self.sent.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl Transport for RecordingTransport {
    type Body = String;

    fn send(
        &self,
        target: NodeId,
        request: Request<Self::Body>,
    ) -> BoxFuture<'_, Result<Response<Self::Body>>> {
        Box::pin(async move {
            let hops = ForwardMeta::hops_from_headers(request.headers());
            let deadline_ms = ForwardMeta::deadline_from_headers(request.headers())
                .map(|deadline| deadline.as_millis());
            if let Ok(mut guard) = self.sent.lock() {
                guard.push(RecordedForward {
                    target: target.clone(),
                    hops,
                    deadline_ms,
                });
            }
            let mut response = Response::new(format!("forwarded to {target}"));
            *response.status_mut() = http::StatusCode::OK;
            Ok(response)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn hop_limit_refuses_the_next_hop() {
        let target = NodeId::new("report-1");
        let meta = ForwardMeta {
            hops: 4,
            deadline: None,
        };
        let err = meta.next(&target, 4).expect_err("hop limit");
        assert_eq!(err.code(), crate::codes::HOP_LIMIT);
        let next = ForwardMeta {
            hops: 3,
            deadline: None,
        }
        .next(&target, 4)
        .expect("one hop left");
        assert_eq!(next.hops, 4);
    }

    #[test]
    fn headers_round_trip() {
        let meta = ForwardMeta {
            hops: 2,
            deadline: Some(Deadline::from_millis(1_500)),
        };
        let mut request = Request::new(String::new());
        meta.apply(&mut request);
        assert_eq!(ForwardMeta::hops_from_headers(request.headers()), 2);
        assert_eq!(
            ForwardMeta::deadline_from_headers(request.headers()).map(Deadline::as_millis),
            Some(1_500)
        );
    }

    #[test]
    fn malformed_headers_read_as_absent() {
        let mut request = Request::new(String::new());
        request.headers_mut().insert(
            HeaderName::from_static(HOP_HEADER),
            HeaderValue::from_static("x"),
        );
        assert_eq!(ForwardMeta::hops_from_headers(request.headers()), 0);
        assert!(ForwardMeta::deadline_from_headers(request.headers()).is_none());
    }

    #[tokio::test]
    async fn client_stamps_headers_and_records() {
        let client = ForwardingClient::new(RecordingTransport::new(), 4);
        let request = Request::new(String::new());
        let meta = ForwardMeta::initial(Some(Deadline::from_millis(2_000)));
        let response = client
            .forward(NodeId::new("report-1"), request, meta)
            .await
            .expect("forwards");
        assert_eq!(response.status(), http::StatusCode::OK);
        let recorded = client.transport().forwarded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].target, NodeId::new("report-1"));
        assert_eq!(recorded[0].hops, 1);
        assert_eq!(recorded[0].deadline_ms, Some(2_000));
    }

    #[tokio::test]
    async fn client_refuses_past_the_budget() {
        let client = ForwardingClient::new(RecordingTransport::new(), 1);
        let meta = ForwardMeta {
            hops: 1,
            deadline: None,
        };
        let err = client
            .forward(NodeId::new("report-1"), Request::new(String::new()), meta)
            .await
            .expect_err("hop limit");
        assert_eq!(err.code(), crate::codes::HOP_LIMIT);
        assert!(client.transport().forwarded().is_empty());
    }
}
