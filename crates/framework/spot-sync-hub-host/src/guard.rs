//! The guard in front of the sync router.
//!
//! Every request passes it before the router's own identity middleware. It resolves the client
//! certificate against the **live** directory (so a node revoked while a connection is open is
//! refused at its next request, and a long pull ends within the long-poll cap), then:
//!
//! 1. admits the request against the per-node and global connection limits (the slot is held
//!    until the response body ends, so a long pull keeps its slot);
//! 2. for `GET /sync/v1/pull`: a stream of the hub's own site must be the node's own queue
//!    `<hub site>#<partition>` for a partition the node owns (the destination id of the relay is
//!    the partition name); naming another node's queue is `sync_http.partition_violation`;
//! 3. for `POST /sync/v1/push`: takes a push token (a node that pushes faster than its rate is
//!    told to wait);
//! 4. for `POST /sync/v1/hello`: a hello that names a node of another site than the certificate's
//!    is `sync_http.partition_violation` (tenancy follows the pinned identity, never the request).
//!
//! Refusals are counted per reason and audited at most once per node per interval; `last_seen`
//! is reported at most once per node per ten seconds.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Query, Request, State};
use axum::http::{Method, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_sync_http::error::{Error, Failure};
use spot_sync_http::{ClientCertificate, NodeDirectory};
use spot_sync_protocol::Hello;
use tokio_stream::StreamExt as _;

use crate::limits::{Admission, AuditThrottle, Limits, Refusal};

/// The most a hello body may weigh.
const MAX_HELLO_BYTES: usize = 16 * 1024;
/// The least time between two `last_seen` reports for one node.
const SEEN_INTERVAL_MS: i64 = 10_000;

/// Where refusals are audited.
pub trait RefusalAudit: Send + Sync {
    /// One refusal of `who` (a node id, or a certificate fingerprint, both public) for `reason`
    /// (a stable code). Called at most once per `who` per interval.
    fn refused(&self, who: &str, reason: &str);
}

/// Where `last_seen` goes.
pub trait SeenSink: Send + Sync {
    /// `node` was seen at `at_ms` (the host's monotonic milliseconds).
    fn seen(&self, node: &str, at_ms: i64);
}

/// What the guard is configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardSettings {
    /// The connection and rate limits.
    pub limits: Limits,
    /// The hub's own site: the site of the streams a node pulls.
    pub hub_site: String,
}

/// The guard's state.
pub struct Guard {
    settings: GuardSettings,
    directory: Arc<dyn NodeDirectory>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn RefusalAudit>,
    seen: Option<Arc<dyn SeenSink>>,
    admission: Arc<Admission>,
    audit_throttle: AuditThrottle,
    seen_throttle: AuditThrottle,
    counts: Mutex<BTreeMap<&'static str, u64>>,
}

impl Guard {
    /// A guard over `directory`; `seen` receives `last_seen` reports.
    #[must_use]
    pub fn new(
        settings: GuardSettings,
        directory: Arc<dyn NodeDirectory>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn RefusalAudit>,
        seen: Option<Arc<dyn SeenSink>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            admission: Admission::new(settings.limits),
            audit_throttle: AuditThrottle::new(settings.limits.audit_interval_ms),
            seen_throttle: AuditThrottle::new(SEEN_INTERVAL_MS),
            settings,
            directory,
            clock,
            audit,
            seen,
            counts: Mutex::new(BTreeMap::new()),
        })
    }

    fn now_ms(&self) -> i64 {
        i64::try_from(self.clock.monotonic().as_millis()).unwrap_or(i64::MAX)
    }

    /// Refusals counted so far, by reason.
    #[must_use]
    pub fn counts(&self) -> BTreeMap<&'static str, u64> {
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Requests in flight.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.admission.in_flight()
    }

    fn refuse(&self, who: &str, reason: &'static str, now_ms: i64) {
        {
            let mut counts = self
                .counts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *counts.entry(reason).or_insert(0) += 1;
        }
        if self.audit_throttle.should_audit(who, now_ms) {
            self.audit.refused(who, reason);
        }
    }
}

fn failure(error: Error) -> Response {
    Failure::from(error).into_response()
}

fn refusal_error(refusal: Refusal) -> Error {
    Error::unavailable(format!("the hub is busy: {}", refusal.word()))
}

fn pulled_source(uri: &Uri) -> Option<SourceId> {
    let Query(params) = Query::<BTreeMap<String, String>>::try_from_uri(uri).ok()?;
    SourceId::new(params.get("source")?).ok()
}

/// The middleware. Mount with [`guard_layer`].
pub async fn run(State(guard): State<Arc<Guard>>, request: Request, next: Next) -> Response {
    let now = guard.now_ms();
    // No certificate: the router's own identity middleware refuses it with the registered code.
    let Some(certificate) = request.extensions().get::<ClientCertificate>().cloned() else {
        return next.run(request).await;
    };
    let node = match guard.directory.resolve(&certificate) {
        Ok(node) => node,
        Err(error) => {
            let code = error.code;
            guard.refuse(certificate.fingerprint(), code, now);
            return failure(error);
        }
    };
    let name = node.node.as_str().to_owned();

    let permit = match guard.admission.enter(&name, now) {
        Ok(permit) => permit,
        Err(refusal) => {
            guard.refuse(&name, refusal.word(), now);
            return failure(refusal_error(refusal));
        }
    };
    if let Some(seen) = &guard.seen
        && guard.seen_throttle.should_audit(&name, now)
    {
        seen.seen(&name, now);
    }

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let mut request = request;
    match (method, path.as_str()) {
        (Method::GET, "/sync/v1/pull") => {
            if let Some(source) = pulled_source(request.uri()) {
                let own_site = source.site() == guard.settings.hub_site;
                let own_queue = source
                    .partition()
                    .is_some_and(|partition| node.partitions.contains(partition));
                if own_site && !own_queue {
                    let error = Error::partition_violation(&name, source.as_str());
                    guard.refuse(&name, error.code, now);
                    return failure(error);
                }
            }
        }
        (Method::POST, "/sync/v1/push") => {
            if let Err(refusal) = guard.admission.allow_push(&name, now) {
                guard.refuse(&name, refusal.word(), now);
                return failure(refusal_error(refusal));
            }
        }
        (Method::POST, "/sync/v1/hello") => {
            let (parts, body) = request.into_parts();
            let bytes = match to_bytes(body, MAX_HELLO_BYTES).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    let error = Error::invalid_request("the hello body is too large or unreadable");
                    guard.refuse(&name, error.code, now);
                    return failure(error);
                }
            };
            if let Ok(hello) = serde_json::from_slice::<Hello>(&bytes)
                && hello.node.site() != node.node.site()
            {
                let error = Error::partition_violation(&name, hello.node.as_str());
                guard.refuse(&name, error.code, now);
                return failure(error);
            }
            request = Request::from_parts(parts, Body::from(bytes));
        }
        _ => {}
    }

    let response = next.run(request).await;
    // The slot is held until the body ends: a long pull keeps it, a finished one frees it.
    let (parts, body) = response.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        let _slot = &permit;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Puts the guard in front of `router`.
pub fn guard_layer(router: Router, guard: Arc<Guard>) -> Router {
    router.layer(middleware::from_fn_with_state(guard, run))
}
