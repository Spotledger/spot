//! The gate: a middleware that holds the acknowledgement of a mutating request until the journal
//! is durable up to the outbox head read **after** the handler returned.
//!
//! # Why the head read after the commit
//!
//! The storage contract's `commit()` returns no position (contract 0.1.0 is frozen), so the gate
//! reads the outbox head after the handler succeeded. That head is at or above the position of
//! the request's own events; it may also cover entries of concurrent transactions, which is safe
//! (the gate may wait for slightly more than it needs, never less) and cheap under group commit.
//!
//! # What it does, per policy
//!
//! - **No journal, or `local`**: nothing to wait for; the local commit is the durability.
//! - **`remote-sync` / `replica-sync`**: after a success status, read the head; if it is above
//!   the journal's durable position, wait for it or `wait` (default 5 s), then answer.
//! - **Store unreachable, `degrade`**: acknowledge on the local commit (no wait once the shipper
//!   knows the store is down; after `wait` otherwise); the backlog is the shipper's, and so is
//!   `RPO_BREACH`.
//! - **Store unreachable, `halt`**: a new mutating request is refused **before the handler runs**
//!   with `503 server.unavailable`, `Retry-After` and `details.code = "journal.halted"`; a request
//!   that already committed and could not be confirmed answers `503` with
//!   `details.code = "journal.commit_outcome_unknown"`, retryable: the client repeats with the same
//!   `Idempotency-Key` and the idempotency layer, which sits inside the gate, replays the original
//!   result.
//! - **Reads** (`GET`, `HEAD`, `OPTIONS`) and every non-success answer pass untouched.
//!
//! The gate matches the HTTP method of the routed request: it is installed with `route_layer`, so
//! it runs after routing and a method-override header or a path trick cannot make a write look
//! like a read. Fail closed: when the head or the durable position cannot be read the commit is
//! treated as unconfirmed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::policy::{Kind, OnUnreachable, PolicyCell};
use crate::stats::Stats;

/// The boxed future of a port call.
pub type GateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The outbox head, read after a commit.
pub trait HeadSource: Send + Sync {
    /// `(generation, seq)` of the newest published outbox entry, or `None` when it cannot be read.
    fn head(&self) -> GateFuture<'_, Option<(u64, u64)>>;
}

/// The journal as the gate sees it.
pub trait Journal: Send + Sync {
    /// The last position made durable.
    fn durable(&self) -> (u64, u64);
    /// Waits until `position` is durable, up to `timeout`.
    fn wait_durable(&self, position: (u64, u64), timeout: Duration) -> GateFuture<'_, bool>;
    /// Whether the shipper knows the store cannot be reached (it is failing to ship).
    fn unreachable(&self) -> bool;
}

/// The standby's confirmation under `replica-sync`.
pub trait Standby: Send + Sync {
    /// Waits until the standby has confirmed `position`, up to `timeout`.
    fn wait_confirmed(&self, position: (u64, u64), timeout: Duration) -> GateFuture<'_, bool>;
}

/// The gate's settings.
#[derive(Clone, Copy, Debug)]
pub struct GateConfig {
    /// How long a committed write waits for the journal (default 5 s).
    pub wait: Duration,
    /// The `Retry-After` of a refusal, in seconds (default 5).
    pub retry_after_secs: u64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            wait: Duration::from_secs(5),
            retry_after_secs: 5,
        }
    }
}

/// Everything the middleware reads. One per site.
pub struct GateState {
    /// The policy in force.
    pub policy: Arc<PolicyCell>,
    /// Where the outbox head is read.
    pub head: Arc<dyn HeadSource>,
    /// The journal.
    pub journal: Arc<dyn Journal>,
    /// The standby, for `replica-sync`.
    pub standby: Option<Arc<dyn Standby>>,
    /// The settings.
    pub config: GateConfig,
    /// What the gate counted.
    pub stats: Arc<Stats>,
}

fn mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn refusal(detail: &str, message: &str, retry_after: u64, request_id: &str) -> Response {
    let body = serde_json::json!({ "error": {
        "code": "server.unavailable",
        "message": message,
        "retryable": true,
        "request_id": request_id,
        "details": { "code": detail, "retry_after_seconds": retry_after },
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        headers.insert(header::RETRY_AFTER, value);
    }
    response
}

/// The middleware. Install it with `router.route_layer(from_fn_with_state(state, gate))`.
pub async fn gate(State(state): State<Arc<GateState>>, request: Request, next: Next) -> Response {
    if !mutating(request.method()) {
        return next.run(request).await;
    }
    let policy = state.policy.current();
    if !policy.journal_on {
        return next.run(request).await;
    }
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let halting = policy.kind != Kind::Local && policy.on_unreachable == OnUnreachable::Halt;
    // Halt: refuse before the handler runs while the store is known to be unreachable.
    if halting && state.journal.unreachable() {
        state.stats.refused_halted();
        return refusal(
            "journal.halted",
            "The durability policy refuses new writes while the journal store is unreachable.",
            state.config.retry_after_secs,
            &request_id,
        );
    }
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    if policy.kind == Kind::Local {
        state.stats.local_commit_durable();
        return response;
    }
    // Degrade with a store the shipper knows is down: acknowledge on the local commit now.
    if !halting && state.journal.unreachable() {
        state.stats.acked_local();
        return response;
    }
    let started = Instant::now();
    let confirmed = confirm(&state, policy.kind).await;
    state.stats.waited(started.elapsed());
    if confirmed {
        return response;
    }
    if halting {
        state.stats.outcome_unknown();
        return refusal(
            "journal.commit_outcome_unknown",
            "The change is committed here but the journal did not confirm it in time; retry with the same Idempotency-Key.",
            state.config.retry_after_secs,
            &request_id,
        );
    }
    state.stats.acked_local();
    response
}

/// Whether the journal (or the standby) holds everything up to the head read now.
async fn confirm(state: &GateState, kind: Kind) -> bool {
    let Some(head) = state.head.head().await else {
        // Fail closed: a head that cannot be read is an unconfirmed commit.
        return false;
    };
    match kind {
        Kind::Local => true,
        Kind::RemoteSync => {
            if state.journal.durable() >= head {
                return true;
            }
            state.journal.wait_durable(head, state.config.wait).await
        }
        Kind::ReplicaSync => match &state.standby {
            Some(standby) => standby.wait_confirmed(head, state.config.wait).await,
            None => false,
        },
    }
}
