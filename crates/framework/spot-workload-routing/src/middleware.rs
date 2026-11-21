//! The `spot-http` middleware: a tower layer that consumes the routing decision.
//!
//! [`RoutingLayer`] wraps any tower service that serves `http::Request<B>` and answers
//! `http::Response<B>` — exactly the shape `spot-http`'s axum stack uses. On each request it:
//!
//! - builds a [`RequestMeta`] from the method, path and headers,
//! - asks the [`RoutingEngine`] where the request runs,
//! - for [`RouteDecision::Local`] or [`RouteDecision::Replica`], inserts the [`RouteOutcome`] and
//!   the [`ReadPreference`] into the request extensions and calls the inner service,
//! - for [`RouteDecision::Forward`], stamps the next hop count and the deadline
//!   ([`ForwardMeta::apply`]) and hands the request to the [`Transport`], so two nodes can never
//!   bounce a request past the policy's `max_hops`.
//!
//! The layer performs no I/O itself and reads no clock: the engine, the session source and the
//! transport are injected, so a test drives it with fakes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderMap, Request, Response};
use tower::{Layer, Service};

use crate::error::Error;
use crate::forward::{ForwardMeta, Transport};
use crate::node::NodeId;
use crate::read::{ReadPreference, SessionReadState};
use crate::route::{RequestMeta, RouteDecision, RouteOutcome, RoutingEngine};

/// A source of the caller session's read-your-writes state. The host wires its session store here;
/// a host without sessions uses [`NoSessions`].
pub trait SessionSource: Send + Sync {
    /// The session's state for this request, from its headers (or from a server-side lookup).
    fn read_state(&self, headers: &HeaderMap) -> SessionReadState;
}

/// A session source that never reports a write: every read may use a replica within its lag.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSessions;

impl SessionSource for NoSessions {
    fn read_state(&self, _headers: &HeaderMap) -> SessionReadState {
        SessionReadState::none()
    }
}

/// A session source backed by a closure, for tests and small hosts.
pub struct FnSessions<F>(pub F);

impl<F> SessionSource for FnSessions<F>
where
    F: Fn(&HeaderMap) -> SessionReadState + Send + Sync,
{
    fn read_state(&self, headers: &HeaderMap) -> SessionReadState {
        (self.0)(headers)
    }
}

/// Build the request metadata the engine decides on: the method decides whether the request
/// carries a write, the headers carry the hop count and deadline, and the session source supplies
/// the read-your-writes state. The request's own class is [`crate::class::WorkloadClass::Interactive`]
/// until a path rule overrides it — the policy is the source of truth for class.
#[must_use]
pub fn meta_from_parts(
    method: &http::Method,
    path: &str,
    headers: &HeaderMap,
    sessions: &dyn SessionSource,
) -> RequestMeta {
    let is_write = !matches!(
        *method,
        http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
    );
    RequestMeta {
        path: path.to_owned(),
        route: None,
        class: crate::class::WorkloadClass::Interactive,
        is_write,
        hops: ForwardMeta::hops_from_headers(headers),
        deadline: ForwardMeta::deadline_from_headers(headers),
        session: sessions.read_state(headers),
    }
}

/// A routing middleware failure: the inner service failed, or the transport could not forward.
#[derive(Debug)]
pub enum MiddlewareError<E> {
    /// The inner service failed.
    Inner(E),
    /// The forwarding transport failed.
    Forward(Error),
}

impl<E: std::fmt::Display> std::fmt::Display for MiddlewareError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MiddlewareError::Inner(e) => write!(f, "inner service failed: {e}"),
            MiddlewareError::Forward(e) => write!(f, "forwarding failed: {e}"),
        }
    }
}

/// The tower layer.
pub struct RoutingLayer<F> {
    engine: Arc<RoutingEngine>,
    sessions: Arc<dyn SessionSource>,
    transport: Arc<F>,
    max_hops: u32,
}

impl<F> Clone for RoutingLayer<F> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            sessions: Arc::clone(&self.sessions),
            transport: Arc::clone(&self.transport),
            max_hops: self.max_hops,
        }
    }
}

impl<F> RoutingLayer<F> {
    /// A layer over the engine, the session source and the forwarding transport. `max_hops` bounds
    /// forwarding even when a path rule sets no budget of its own.
    #[must_use]
    pub fn new(
        engine: Arc<RoutingEngine>,
        sessions: Arc<dyn SessionSource>,
        transport: Arc<F>,
        max_hops: u32,
    ) -> Self {
        Self {
            engine,
            sessions,
            transport,
            max_hops,
        }
    }
}

impl<S, F> Layer<S> for RoutingLayer<F> {
    type Service = RoutingService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        RoutingService {
            inner,
            engine: Arc::clone(&self.engine),
            sessions: Arc::clone(&self.sessions),
            transport: Arc::clone(&self.transport),
            max_hops: self.max_hops,
        }
    }
}

/// The service [`RoutingLayer`] produces.
pub struct RoutingService<S, F> {
    inner: S,
    engine: Arc<RoutingEngine>,
    sessions: Arc<dyn SessionSource>,
    transport: Arc<F>,
    max_hops: u32,
}

impl<S, F, B> Service<Request<B>> for RoutingService<S, F>
where
    S: Service<Request<B>, Response = Response<B>>,
    F: Transport<Body = B> + Send + Sync + 'static,
    B: Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<B>;
    type Error = MiddlewareError<S::Error>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(MiddlewareError::Inner)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        let meta = meta_from_parts(
            request.method(),
            request.uri().path(),
            request.headers(),
            &*self.sessions,
        );
        let outcome = self.engine.decide(&meta);
        match outcome.decision.clone() {
            RouteDecision::Forward(target) => {
                let forward_meta = ForwardMeta {
                    hops: meta.hops,
                    deadline: meta.deadline,
                };
                let next = match forward_meta.next(&target, self.max_hops) {
                    Ok(next) => next,
                    Err(error) => {
                        self.engine.record_forward_failed(&target);
                        return Box::pin(async move { Err(MiddlewareError::Forward(error)) });
                    }
                };
                next.apply(&mut request);
                let transport = Arc::clone(&self.transport);
                let engine = Arc::clone(&self.engine);
                Box::pin(async move {
                    match transport.send(target.clone(), request).await {
                        Ok(response) => Ok(response),
                        Err(error) => {
                            engine.record_forward_failed(&target);
                            Err(MiddlewareError::Forward(error))
                        }
                    }
                })
            }
            _ => {
                request.extensions_mut().insert(outcome.clone());
                request.extensions_mut().insert(read_preference(&outcome));
                let future = self.inner.call(request);
                Box::pin(async move { future.await.map_err(MiddlewareError::Inner) })
            }
        }
    }
}

/// The read preference the request context carries for the storage layer to honour.
fn read_preference(outcome: &RouteOutcome) -> ReadPreference {
    match outcome.decision {
        RouteDecision::Replica { max_lag } => ReadPreference::Replica { max_lag },
        _ => outcome.read.preference,
    }
}

/// The node a request was forwarded to, when the layer forwarded it. Used by tests and by a host
/// that wants to log the decision.
#[must_use]
pub fn forwarded_target(outcome: &RouteOutcome) -> Option<&NodeId> {
    outcome.decision.target()
}
