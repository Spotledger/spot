//! The `/api/v1` middlewares: `auth_required` (bearer credential must be present on
//! secured routes) and the `Idempotency-Key` protocol for commands that declare the key,
//! plus the replay renderer both paths share.

use crate::api::idempotency::{
    IdempotencyError, IdempotencyStore, KeyScope, Reservation, StoredResponse, fingerprint,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, Credential};
use crate::request::RequestId;
use crate::site::SiteContext;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value as Json;
use spot_core::Clock;

use super::routes::RateClass;
use super::{
    MAX_READ_BODY, REPLAYED_HEADER, SharedState, ValidatedKey, credential_hash, render_error,
};
/// `Authorization: Bearer <token>` extraction; a secured route without it answers
/// `401 auth.required` before anything runs. The token is not validated here — the services
/// do that (WP-PLT-02).
pub(super) async fn auth_middleware(request: Request, next: Next) -> Response {
    if header_credential(request.headers()).is_some() {
        return next.run(request).await;
    }
    let request_id = request_id_of(&request);
    render_error(ApiError::from_code(Code::AuthRequired), &request_id)
}

/// Requests one client address may make in a rate class per window.
const AUTH_LIMIT: u32 = 20;
/// The fixed window of every class.
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Most distinct client keys held at once. When the table is full of live windows a new key is
/// refused (fail closed) rather than the table growing without bound.
const MAX_RATE_KEYS: usize = 65_536;

/// A client key: the address (an IPv6 address is reduced to its /64, the unit one subscriber
/// holds, and an IPv4-mapped one to its IPv4 address) or `Unknown` for a request without one.
/// Unknown requests share one bucket, so a missing address never means no limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ClientKey {
    Addr(IpAddr),
    Unknown,
}

impl ClientKey {
    fn of(peer: Option<IpAddr>) -> Self {
        match peer {
            None => ClientKey::Unknown,
            Some(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
                Some(v4) => ClientKey::Addr(IpAddr::V4(v4)),
                None => {
                    let mut octets = v6.octets();
                    octets[8..].fill(0);
                    ClientKey::Addr(IpAddr::V6(Ipv6Addr::from(octets)))
                }
            },
            Some(v4) => ClientKey::Addr(v4),
        }
    }
}

/// A small in-memory fixed-window limiter keyed by (rate class, client address), timed by the
/// server's injected clock (monotonic, never the wall clock). One counter per class and client
/// address, shared by every public group of that class.
pub(super) struct RateLimiter {
    clock: Arc<dyn Clock>,
    windows: Mutex<HashMap<(RateClass, ClientKey), (Duration, u32)>>,
}

impl RateLimiter {
    pub(super) fn new(clock: Arc<dyn Clock>) -> Self {
        RateLimiter {
            clock,
            windows: Mutex::new(HashMap::new()),
        }
    }

    fn limit(class: RateClass) -> u32 {
        match class {
            RateClass::Auth => AUTH_LIMIT,
        }
    }

    /// Counts one request; `Err(seconds)` is the wait until the window ends.
    fn check(&self, class: RateClass, peer: Option<IpAddr>) -> Result<(), u64> {
        let now = self.clock.monotonic();
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let key = (class, ClientKey::of(peer));
        let wait = |start: Duration| {
            (start + RATE_WINDOW)
                .saturating_sub(now)
                .as_secs()
                .saturating_add(1)
        };
        if let Some((start, count)) = windows.get_mut(&key) {
            if now.saturating_sub(*start) >= RATE_WINDOW {
                *start = now;
                *count = 1;
                return Ok(());
            }
            if *count >= Self::limit(class) {
                return Err(wait(*start));
            }
            *count += 1;
            return Ok(());
        }
        if windows.len() >= MAX_RATE_KEYS {
            windows.retain(|_, (start, _)| now.saturating_sub(*start) < RATE_WINDOW);
            if windows.len() >= MAX_RATE_KEYS {
                return Err(RATE_WINDOW.as_secs());
            }
        }
        windows.insert(key, (now, 1));
        Ok(())
    }
}

/// What the rate layer of one public group carries.
#[derive(Clone)]
pub(super) struct RateGuard {
    pub(super) limiter: Arc<RateLimiter>,
    pub(super) class: RateClass,
}

/// The layer of a public group: the rate limit first (before any handler work, so before a
/// cookie is read), then the `Authorization` header is removed so the handler cannot learn or
/// use a caller identity.
pub(super) async fn public_middleware(
    axum::extract::State(guard): axum::extract::State<RateGuard>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<SiteContext>()
        .and_then(|ctx| ctx.peer);
    if let Err(wait) = guard.limiter.check(guard.class, peer) {
        let request_id = request_id_of(&request);
        return render_error(
            ApiError::from_code(Code::ApiRateLimited).detail("retry_after", Json::from(wait)),
            &request_id,
        );
    }
    request.headers_mut().remove(header::AUTHORIZATION);
    next.run(request).await
}

pub(super) fn header_credential(headers: &HeaderMap) -> Option<Credential> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        return None;
    }
    Some(Credential::new(token))
}

pub(super) fn request_id_of(request: &Request) -> RequestId {
    request
        .extensions()
        .get::<SiteContext>()
        .map(|ctx| ctx.request_id.clone())
        .or_else(|| request.extensions().get::<RequestId>().cloned())
        .unwrap_or_else(RequestId::generate)
}

/// What a route lets the store keep (WP-PLT-05.T7, the approved design).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Persist {
    /// The response is stored and replayed: document lifecycle bodies (already masked by the
    /// caller's read permission) and the file-upload record.
    Body,
    /// The response is never written. The key is still reserved while the command runs, so a
    /// concurrent duplicate waits instead of running twice, and is then released: a response
    /// that app code decides (a token, a signing secret, an export) is data the store must
    /// not hold at rest.
    None,
}

/// The retention class of the commands group (a site configures what it means).
pub(crate) const CLASS_HTTP_API: &str = "http_api";
/// The retention class of the file upload.
pub(crate) const CLASS_HTTP_UPLOAD: &str = "http_upload";

/// The commands group, route by route, and what each may persist. A route that is not in this
/// table is treated as [`Persist::None`], so a new route can never start storing bodies by
/// omission; `tests/idempotency_fail_closed.rs` fails when the group and a table of its own
/// disagree.
const COMMAND_ROUTES: [(&str, Persist); 7] = [
    ("/api/v1/resource/{doctype}", Persist::Body),
    ("/api/v1/resource/{doctype}/{id}/post", Persist::Body),
    ("/api/v1/resource/{doctype}/{id}/cancel", Persist::Body),
    ("/api/v1/resource/{doctype}/{id}/correct", Persist::Body),
    ("/api/v1/resource/{doctype}/{id}/rename", Persist::Body),
    (
        "/api/v1/resource/{doctype}/{id}/methods/{method}",
        Persist::None,
    ),
    ("/api/v1/method/{app}/{module}/{function}", Persist::None),
];

/// The persistence of the matched route pattern; unknown is [`Persist::None`].
fn persistence_of(pattern: Option<&str>) -> Persist {
    pattern
        .and_then(|pattern| COMMAND_ROUTES.iter().find(|(p, _)| *p == pattern))
        .map_or(Persist::None, |(_, persist)| *persist)
}

/// How many times a duplicate may find its key pending and wait before the router gives up
/// with 503. Each round is one `wait_pending` (itself bounded by the store's wait budget).
const MAX_WAIT_ROUNDS: usize = 8;

/// The 503 the router sends when the store cannot vouch for a key: the same
/// `server.unavailable` and `Retry-After` the storage outages use.
pub(super) fn store_unavailable(error: IdempotencyError) -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail(
        crate::api::storage_error::RETRY_AFTER_DETAIL,
        Json::from(error.retry_after_secs().max(1)),
    )
}

/// What the reservation loop decided.
pub(super) enum Admission {
    /// This caller holds the reservation and must run the request.
    Run,
    /// A stored response with the same fingerprint: replay it.
    Replay(StoredResponse),
    /// The key was used with a different request: `409 api.idempotency_conflict`.
    Conflict,
}

/// Reserves `scope` for `fingerprint`, waiting out an in-flight duplicate by re-reading the
/// store. A store failure or an exhausted wait is an `Err` the caller answers with 503 **before
/// running anything**; the duplicate is never run a second time and never answered 409.
pub(super) async fn admit(
    store: &dyn IdempotencyStore,
    scope: &KeyScope,
    fingerprint: &str,
) -> Result<Admission, ApiError> {
    for _ in 0..MAX_WAIT_ROUNDS {
        match store
            .reserve(scope, fingerprint)
            .await
            .map_err(store_unavailable)?
        {
            Reservation::Fresh => return Ok(Admission::Run),
            Reservation::Completed(stored) => return Ok(Admission::Replay(stored)),
            Reservation::Conflict => return Ok(Admission::Conflict),
            Reservation::Pending => store.wait_pending(scope).await.map_err(store_unavailable)?,
        }
    }
    Err(store_unavailable(IdempotencyError::unavailable()))
}

/// How a finished command is recorded.
#[derive(Clone, Copy)]
pub(super) struct Outcome<'a> {
    /// What the route lets the store keep.
    pub(super) persist: Persist,
    /// The retention class of the route group (a constant, never taken from the request).
    pub(super) class: &'a str,
    /// The status must not be remembered: release the key so a retry runs again.
    pub(super) release: bool,
}

/// Records the outcome of a reserved command: stores the response, or releases the key.
///
/// `release` is the caller's rule for statuses that must not be remembered. A response is also
/// released (never written) when the route is [`Persist::None`] or the body is larger than the
/// store keeps. A `complete` that fails after the command ran is logged without the key or the
/// body and the real response is still returned: the store's lease makes a retry find an
/// expired pending row (the documented at-least-once edge, see `api::idempotency`).
pub(super) async fn settle(
    store: &dyn IdempotencyStore,
    scope: &KeyScope,
    fingerprint: &str,
    outcome: Outcome<'_>,
    parts: &axum::http::response::Parts,
    body: &[u8],
) {
    let Outcome {
        persist,
        class,
        release,
    } = outcome;
    if release || persist == Persist::None {
        store.fail(scope, fingerprint).await;
        return;
    }
    if body.len() > store.max_body_bytes() {
        tracing::warn!(
            bytes = body.len(),
            "response larger than the idempotency store keeps; not stored, key released"
        );
        store.fail(scope, fingerprint).await;
        return;
    }
    let stored = StoredResponse {
        status: parts.status.as_u16(),
        headers: stored_headers(&parts.headers),
        body: body.to_vec(),
    };
    if store
        .complete(scope, fingerprint, stored, class)
        .await
        .is_err()
    {
        tracing::warn!("the idempotency store could not record a completed response");
    }
}

/// The `Idempotency-Key` protocol (ADR-055) on the commands that declare it. Malformed keys
/// answer `400 api.invalid_request`; content type and JSON syntax are checked **before** the
/// key is reserved, so a rejected request never blocks a later retry. Completed responses are
/// stored (document lifecycle bodies only, see [`Persist`]); a `5xx`, a `412`/`428`, or a
/// cancelled request releases the reservation so a retry re-runs the command. A second request
/// with the same in-flight key **waits** and then replays (documented choice; see
/// `api::idempotency`). When the store is down the request is answered `503 server.unavailable`
/// and never executed: a keyed request does not run unreserved.
pub(super) async fn idempotency_middleware(
    axum::extract::State(state): axum::extract::State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request_id_of(&request);
    let Some(raw_key) = request.headers().get("idempotency-key") else {
        return next.run(request).await;
    };
    let bad_key = || {
        render_error(
            ApiError::new(
                Code::ApiInvalidRequest,
                "The Idempotency-Key header must be 1 to 128 visible ASCII characters.",
            ),
            &request_id,
        )
    };
    let Ok(key) = raw_key.to_str() else {
        return bad_key();
    };
    let key_valid =
        !key.is_empty() && key.len() <= 128 && key.bytes().all(|b| (0x21..=0x7e).contains(&b));
    if !key_valid {
        return bad_key();
    }
    let key = key.to_owned();
    let persist = persistence_of(
        request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
    );
    let (mut parts, body) = request.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_owned();
    let bytes = match axum::body::to_bytes(body, MAX_READ_BODY).await {
        Ok(bytes) => bytes.to_vec(),
        Err(_) => {
            return render_error(ApiError::from_code(Code::HttpBodyTooLarge), &request_id);
        }
    };
    // Content type and JSON syntax before reserving the key.
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if let Some(ct) = &content_type
        && ct != "application/json"
    {
        return render_error(
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            &request_id,
        );
    }
    // A non-empty body must be valid JSON *before* the key is reserved, so a rejected request
    // never holds the key. An empty body is left to the handler (`400` on required-body
    // routes, fine on optional ones).
    if !bytes.is_empty() && serde_json::from_slice::<Json>(&bytes).is_err() {
        return render_error(ApiError::from_code(Code::ApiInvalidJson), &request_id);
    }
    let credential = header_credential(&parts.headers);
    let site = parts
        .extensions
        .get::<SiteContext>()
        .map(|ctx| ctx.site.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let scope = KeyScope {
        site,
        credential: credential_hash(credential.as_ref()),
        key: key.clone(),
    };
    let fp = fingerprint(method.as_str(), &path, &bytes);
    match admit(&*state.idempotency, &scope, &fp).await {
        Err(error) => return render_error(error, &request_id),
        Ok(Admission::Conflict) => {
            return render_error(
                ApiError::from_code(Code::ApiIdempotencyConflict),
                &request_id,
            );
        }
        Ok(Admission::Replay(stored)) => return replay_response(&stored, &request_id),
        Ok(Admission::Run) => {}
    }
    parts.extensions.insert(ValidatedKey(key));
    // Releases the reservation if this future is dropped (client disconnect, timeout) before
    // the outcome is recorded, so a retry with the same key runs fresh instead of waiting on
    // an entry nobody will complete.
    let mut guard = ReservationGuard {
        store: Arc::clone(&state.idempotency),
        scope: scope.clone(),
        fingerprint: fp.clone(),
        armed: true,
    };
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    let response = next.run(request).await;
    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    // 5xx, and the precondition failures 412/428, are not stored: `If-Match` is not part of
    // the fingerprint, so replaying a stale-version 412 would answer a retry that carries a
    // fresh `If-Match` with the old failure for 24 h. `400` and `415` are released as well:
    // a malformed request is a request-repair case, and the brief's rule is that a rejected
    // request never holds the key — the handler answers 400 for an empty body on a required
    // route, which the middleware cannot know before the key is reserved.
    // `401` is released too: the bearer is only checked for presence before this layer, so a
    // bogus token would otherwise write one durable row per unauthenticated request.
    let release = status.is_server_error()
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::BAD_REQUEST
        || status == StatusCode::UNSUPPORTED_MEDIA_TYPE
        || status == StatusCode::PRECONDITION_FAILED
        || status == StatusCode::PRECONDITION_REQUIRED;
    settle(
        &*state.idempotency,
        &scope,
        &fp,
        Outcome {
            persist,
            class: CLASS_HTTP_API,
            release,
        },
        &parts,
        &bytes,
    )
    .await;
    guard.armed = false;
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// Releases the reservation on drop while armed (a cancelled request). The release runs on the
/// current runtime; with none, the store's lease frees the key.
pub(super) struct ReservationGuard {
    pub(super) store: Arc<dyn IdempotencyStore>,
    pub(super) scope: KeyScope,
    pub(super) fingerprint: String,
    pub(super) armed: bool,
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let scope = self.scope.clone();
        let fingerprint = std::mem::take(&mut self.fingerprint);
        runtime.spawn(async move {
            store.fail(&scope, &fingerprint).await;
        });
    }
}

/// Headers worth replaying verbatim: what the original produced. `x-request-id` and `date`
/// are per-response and re-stamped live, so they are not stored.
pub(super) fn stored_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    const KEEP: [&str; 4] = ["etag", "location", "content-type", "retry-after"];
    headers
        .iter()
        .filter(|(name, _)| KEEP.contains(&name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

pub(super) fn replay_response(stored: &StoredResponse, request_id: &RequestId) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(stored.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in &stored.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(n, v);
        }
    }
    let result = builder
        .header(
            HeaderName::from_static(REPLAYED_HEADER),
            HeaderValue::from_static("true"),
        )
        .body(axum::body::Body::from(stored.body.clone()));
    match result {
        Ok(response) => response,
        Err(_) => render_error(ApiError::from_code(Code::HttpInternal), request_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::idempotency::{MemoryIdempotencyStore, Reservation};
    use spot_core::ManualClock;
    use time::OffsetDateTime;

    #[tokio::test]
    async fn dropped_guard_releases_the_reservation_but_a_disarmed_one_does_not() {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let store = Arc::new(MemoryIdempotencyStore::new(clock));
        let scope = KeyScope {
            site: "acme".to_owned(),
            credential: "tok".to_owned(),
            key: "k".to_owned(),
        };
        let reserve = || async { store.reserve(&scope, "fp").await };
        assert_eq!(reserve().await, Ok(Reservation::Fresh));
        let guard = ReservationGuard {
            store: store.clone(),
            scope: scope.clone(),
            fingerprint: "fp".to_owned(),
            armed: true,
        };
        drop(guard); // a cancelled request
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(reserve().await, Ok(Reservation::Fresh));
        let mut guard = ReservationGuard {
            store: store.clone(),
            scope: scope.clone(),
            fingerprint: "fp".to_owned(),
            armed: true,
        };
        guard.armed = false;
        drop(guard);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(reserve().await, Ok(Reservation::Pending));
    }

    #[test]
    fn an_unlisted_route_never_persists_a_body() {
        assert_eq!(persistence_of(None), Persist::None);
        assert_eq!(persistence_of(Some("/api/v1/something/new")), Persist::None);
        assert_eq!(
            persistence_of(Some("/api/v1/resource/{doctype}")),
            Persist::Body
        );
        assert_eq!(
            persistence_of(Some("/api/v1/method/{app}/{module}/{function}")),
            Persist::None
        );
    }

    #[test]
    fn the_unavailable_answer_carries_at_least_one_second() {
        let error = store_unavailable(IdempotencyError::Unavailable {
            retry_after_secs: 0,
        });
        assert_eq!(crate::api::storage_error::retry_after_secs(&error), Some(1));
    }
}
