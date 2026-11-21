//! The enrolment endpoint of the hub: `POST /sync/v1/enrol` (WP-RTB-02.T13; ADR-062 section 3).
//!
//! A node that has never talked to the hub holds a one-time code and a self-signed certificate. It
//! has no identity the hub knows yet, so this is the **one open path** of the sync listener: it is
//! authenticated by the one-time code alone, sits outside the client-certificate requirement and
//! the guard's per-node admission, and has its own throttle. The shape is the one the node's
//! `HttpEnroller` calls: a JSON body `{ "node", "code", "certificate" }` with the certificate as
//! hex DER; any 2xx answer means redeemed, anything else carries `{ "error": { "code",
//! "message" } }`.
//!
//! # What the endpoint never does
//!
//! * It never says **why** a code failed: an unknown, expired, used, malformed or wrong-node code is
//!   one answer (`sync_enrol.code_invalid`, 403) with one text, because the redeemer gives it. The
//!   lockout after repeated failures is the redeemer's (`sync_enrol.locked_out`, 429).
//! * It never puts the code, the certificate or the node's key in a log line, an error or a
//!   counter: the code is wiped from the request as soon as it is handed over, and the
//!   `Debug` of every type here shows none of it.
//! * It never blocks the async runtime on the database: redemption is a blocking store call, so it
//!   runs on [`ThreadedRedeemer`]'s own plain thread (the PostgreSQL bridge refuses to be called
//!   from a runtime thread) and the handler awaits the answer.
//!
//! # Throttling
//!
//! A token bucket per second for the whole endpoint ([`EnrolSettings::per_second`]) and a ceiling on
//! requests waiting for the redeemer ([`EnrolSettings::queued`]): an attacker cannot make the hub
//! queue unbounded database work, and a legitimate enrolment (rare, one per node) always gets a
//! token. The body is capped ([`MAX_BODY_BYTES`]) and read with a limit.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use spot_core::Clock;
use tokio::sync::oneshot;

use crate::limits::TokenBucket;

/// The path the endpoint serves.
pub const ENROL_PATH: &str = "/sync/v1/enrol";

/// The most a request body may weigh: a code, a node id and one certificate.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// The answer the redeemer gives for any wrong code (kept in step with `spot-sync-enrol`'s code).
pub const CODE_INVALID: &str = "sync_enrol.code_invalid";
/// The code the redeemer answers with when a source is locked out.
pub const LOCKED_OUT: &str = "sync_enrol.locked_out";
/// The code of a request the endpoint itself refuses (shape, size).
pub const REQUEST_INVALID: &str = "sync_http.invalid_request";
/// The code of a request the endpoint throttles.
pub const THROTTLED: &str = "sync_http.unavailable";

/// A refusal from the redeemer: a stable code, a message without secrets and an HTTP status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stable code (`sync_enrol.*`).
    pub code: String,
    /// The message: the same text for every wrong-code case.
    pub message: String,
    /// The HTTP status to answer with.
    pub status: u16,
}

/// Redeems a one-time code for a node's certificate. The production implementation is the
/// enrolment service of the site (`spot-sync-enrol`); this crate does not depend on it.
pub trait Redeemer: Send + Sync + 'static {
    /// Redeems `code` for `node` with `certificate` (DER). `source` names who asked, for the
    /// failure counter (`sync_enrol` locks a source out after repeated failures).
    ///
    /// # Errors
    /// A [`Refusal`] carrying the redeemer's own code (`sync_enrol.code_invalid` for every wrong
    /// code, `sync_enrol.locked_out`, `sync_enrol.cert_invalid`, a store failure).
    fn redeem(
        &self,
        code: &str,
        node: &str,
        certificate: &[u8],
        source: &str,
    ) -> Result<(), Refusal>;
}

type Job = (
    String,
    String,
    Vec<u8>,
    String,
    oneshot::Sender<Result<(), Refusal>>,
);

/// A [`Redeemer`] that runs on its own plain thread and answers asynchronously.
pub struct ThreadedRedeemer {
    jobs: Mutex<Option<mpsc::SyncSender<Job>>>,
}

impl ThreadedRedeemer {
    /// Starts the thread; every call to [`redeem_async`](Self::redeem_async) runs `inner` there.
    /// `queued` bounds the calls waiting for the thread.
    ///
    /// # Errors
    /// The OS refused the thread.
    pub fn spawn(inner: Arc<dyn Redeemer>, queued: usize) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(queued.max(1));
        std::thread::Builder::new()
            .name("sync-hub-enrol".to_owned())
            .spawn(move || {
                while let Ok((code, node, certificate, source, answer)) = receiver.recv() {
                    let result = inner.redeem(&code, &node, &certificate, &source);
                    // The caller may have gone away; nothing to do then.
                    let _ = answer.send(result);
                }
            })?;
        Ok(Self {
            jobs: Mutex::new(Some(sender)),
        })
    }

    /// Hands one redemption to the thread and awaits the answer. `None` when the queue is full or
    /// the thread is gone (the caller answers "unavailable").
    pub async fn redeem_async(
        &self,
        code: String,
        node: String,
        certificate: Vec<u8>,
        source: String,
    ) -> Option<Result<(), Refusal>> {
        let (answer, wait) = oneshot::channel();
        {
            let guard = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let sender = guard.as_ref()?;
            sender
                .try_send((code, node, certificate, source, answer))
                .ok()?;
        }
        wait.await.ok()
    }

    /// Stops accepting work; the thread ends after the call it is running.
    pub fn close(&self) {
        *self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl std::fmt::Debug for ThreadedRedeemer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedRedeemer").finish_non_exhaustive()
    }
}

/// The endpoint's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnrolSettings {
    /// Requests per second the endpoint accepts in all.
    pub per_second: u32,
    /// Requests that may wait for the redeemer at once.
    pub queued: usize,
}

impl Default for EnrolSettings {
    fn default() -> Self {
        Self {
            per_second: 5,
            queued: 8,
        }
    }
}

struct Endpoint {
    redeemer: ThreadedRedeemer,
    bucket: Mutex<TokenBucket>,
    clock: Arc<dyn Clock>,
}

impl Endpoint {
    fn take_token(&self) -> bool {
        let now =
            i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
        self.bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_take(now)
    }
}

/// The router with the one enrolment route. Merge it **outside** the client-certificate
/// requirement and the guard: the caller has no identity yet.
///
/// # Errors
/// The OS refused the redeemer thread.
pub fn enrol_router(
    redeemer: Arc<dyn Redeemer>,
    settings: EnrolSettings,
    clock: Arc<dyn Clock>,
) -> std::io::Result<Router> {
    let now = i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
    let endpoint = Arc::new(Endpoint {
        redeemer: ThreadedRedeemer::spawn(redeemer, settings.queued)?,
        bucket: Mutex::new(TokenBucket::new(settings.per_second.max(1), now)),
        clock,
    });
    Ok(Router::new()
        .route(ENROL_PATH, post(handle))
        .with_state(endpoint))
}

fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "error": { "code": code, "message": message } }).to_string();
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(body),
    )
        .into_response()
}

/// Lower- or upper-case hex to bytes; `None` for odd length or a non-hex digit.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

async fn handle(State(endpoint): State<Arc<Endpoint>>, request: Request) -> Response {
    if !endpoint.take_token() {
        return refuse(
            StatusCode::TOO_MANY_REQUESTS,
            THROTTLED,
            "the hub is busy; try again shortly",
        );
    }
    let Ok(bytes) = to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            REQUEST_INVALID,
            "the enrolment request is too large",
        );
    };
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EnrolBody {
        node: String,
        code: String,
        certificate: String,
    }
    let Ok(body) = serde_json::from_slice::<EnrolBody>(&bytes) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            REQUEST_INVALID,
            "the enrolment request is not valid",
        );
    };
    let Some(certificate) = unhex(&body.certificate) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            REQUEST_INVALID,
            "the enrolment request is not valid",
        );
    };
    // The failure counter is keyed by the node the request names: the listener does not hand the
    // peer address to a route. The redeemer also counts globally.
    let source = format!("node:{}", body.node);
    let answer = endpoint
        .redeemer
        .redeem_async(body.code, body.node, certificate, source)
        .await;
    match answer {
        Some(Ok(())) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            Body::from("{\"enrolled\":true}"),
        )
            .into_response(),
        Some(Err(refusal)) => {
            let status =
                StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            refuse(status, &refusal.code, &refusal.message)
        }
        None => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            THROTTLED,
            "the hub is busy; try again shortly",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::unhex;

    #[test]
    fn hex_decodes_both_cases_and_refuses_the_rest() {
        assert_eq!(unhex("00ff10Ab"), Some(vec![0x00, 0xff, 0x10, 0xab]));
        assert_eq!(unhex(""), None);
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
