//! The `/api/v1` middlewares: `auth_required` (bearer credential must be present on
//! secured routes) and the `Idempotency-Key` protocol for commands that declare the key,
//! plus the replay renderer both paths share.

use crate::api::idempotency::{
    IdempotencyError, IdempotencyStore, KeyScope, Reservation, StoredResponse, fingerprint,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, AuthService, Credential, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value as Json;
use spot_core::{Audience, Clock};

use super::routes::RateClass;
use super::{
    MAX_READ_BODY, REPLAYED_HEADER, SharedState, ValidatedKey, credential_hash, render_error,
};
use crate::api::routes::{GuestCall, RouteMark, route_table};
/// What the bearer layer carries: the auth port that tells a credential's audience, the clock
/// that times the refusal log's rate bound, and whether a secured route without a
/// [`RouteMark`] is a wiring error (`strict`, every router built from the table) or is let
/// through on bearer presence alone (the legacy `router_with` seam, which tests use to mount
/// their own groups that are not in the table).
#[derive(Clone)]
pub(super) struct AuthGate {
    auth: Arc<dyn AuthService>,
    clock: Arc<dyn Clock>,
    strict: bool,
    logged: Arc<Mutex<HashMap<(Audience, &'static str), Duration>>>,
}

/// One refusal log line per (presented audience, operation) per this long, so a flood of wrong
/// -audience requests cannot fill the log.
const REFUSAL_LOG_WINDOW: Duration = Duration::from_secs(60);

impl AuthGate {
    pub(super) fn new(auth: Arc<dyn AuthService>, clock: Arc<dyn Clock>, strict: bool) -> Self {
        AuthGate {
            auth,
            clock,
            strict,
            logged: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Whether a refusal for `(presented, operation)` may be logged now (and records it).
    fn may_log(&self, presented: Audience, operation: &'static str) -> bool {
        let now = self.clock.monotonic();
        let mut logged = self
            .logged
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match logged.get(&(presented, operation)) {
            Some(last) if now.saturating_sub(*last) < REFUSAL_LOG_WINDOW => false,
            _ => {
                logged.insert((presented, operation), now);
                true
            }
        }
    }
}

/// The bearer layer of every secured route (ADR-083 Barrier 1). In order:
///
/// 1. a missing bearer is `401 auth.required`;
/// 2. the route's [`RouteMark`] (stamped by the outer mark layer) is read; a secured route with
///    no mark is a wiring error: `500 http.internal` and an error log, no handler runs (a
///    non-strict gate, the legacy test seam, skips the audience check for such a route);
/// 3. the auth port says which audience the credential is for (an invalid or expired credential
///    is the port's own 401); an audience the mark does not list is `403 permission.denied`
///    before any handler and before the idempotency middleware, so no key is reserved and no
///    side effect happens. The body does not name audiences.
///
/// Nothing here logs a token, a header or a body.
pub(super) async fn auth_middleware(
    axum::extract::State(gate): axum::extract::State<AuthGate>,
    request: Request,
    next: Next,
) -> Response {
    let Some(credential) = header_credential(request.headers()) else {
        // A function call with no `Authorization` header at all, on a site that serves guest
        // functions, is a guest call: the function service decides it as an anonymous caller,
        // which reaches only functions declared `guest` (WP-PLT-17.T3). Any other secured route
        // still needs its bearer.
        if request.headers().get(header::AUTHORIZATION).is_none()
            && request.extensions().get::<GuestCall>().is_some()
        {
            return next.run(request).await;
        }
        let request_id = request_id_of(&request);
        return render_error(ApiError::from_code(Code::AuthRequired), &request_id);
    };
    let mark = request.extensions().get::<RouteMark>().copied();
    let Some(mark) = mark else {
        // No mark. A request on a path the table knows but with a method it does not list has
        // no handler: the router answers 405 and nothing runs, so it is let through. Any other
        // unmarked secured request is a wiring error in a strict gate.
        let on_known_path = request
            .extensions()
            .get::<MatchedPath>()
            .is_some_and(|path| {
                route_table()
                    .iter()
                    .any(|entry| entry.path == path.as_str())
            });
        if !gate.strict || on_known_path {
            return next.run(request).await;
        }
        tracing::error!("a secured route has no route mark; refusing (wiring error)");
        let request_id = request_id_of(&request);
        return render_error(ApiError::from_code(Code::HttpInternal), &request_id);
    };
    let Some(site) = request.extensions().get::<SiteContext>().cloned() else {
        tracing::error!(
            operation = mark.operation,
            "no site context at the bearer layer"
        );
        let request_id = request_id_of(&request);
        return render_error(ApiError::from_code(Code::HttpInternal), &request_id);
    };
    let ctx = RequestCtx {
        site: site.site.clone(),
        credential: Some(credential),
        idempotency_key: None,
        language: None,
        request_id: site.request_id.clone(),
        peer: site.peer,
    };
    match gate.auth.audience_of(&ctx).await {
        Ok(presented) if mark.audiences.contains(&presented) => next.run(request).await,
        Ok(presented) => {
            if gate.may_log(presented, mark.operation) {
                tracing::warn!(
                    operation = mark.operation,
                    presented = presented.as_str(),
                    allowed = ?mark.audiences.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                    request_id = site.request_id.as_str(),
                    reason = "wrong_audience",
                    "credential refused on a route not marked for its audience"
                );
            }
            render_error(
                ApiError::new(
                    Code::PermissionDenied,
                    "This credential cannot be used on this route.",
                ),
                &site.request_id,
            )
        }
        Err(error) => render_error(error, &site.request_id),
    }
}

/// Requests one client address may make in the `auth` class per window, by default.
pub const DEFAULT_AUTH_LIMIT: u32 = 20;
/// The fixed window of the `auth` class, by default.
pub const DEFAULT_RATE_WINDOW: Duration = Duration::from_secs(60);
/// The largest `auth_rate_limit` a configuration may set: above it a limit is no limit.
pub const MAX_AUTH_LIMIT: u32 = 10_000;
/// The longest `auth_rate_window_seconds` a configuration may set (one hour).
pub const MAX_RATE_WINDOW_SECS: u64 = 3_600;
/// Most live windows the limiter holds in total, across its shards. A full table evicts its
/// oldest window to admit a new key; it never refuses one.
pub const RATE_TABLE_CAPACITY: usize = 65_536;
/// Independent mutex-guarded tables (a power of two, so one lock is never held across all keys).
const RATE_SHARDS: usize = 16;

/// The limits of the rate classes (the `auth` class is the one public groups run in). `Default` is
/// the numbers the limiter always had: 20 requests per 60 seconds per client address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimits {
    /// Requests per window per client address, 1 to [`MAX_AUTH_LIMIT`].
    pub auth_limit: u32,
    /// The fixed window, 1 to [`MAX_RATE_WINDOW_SECS`] seconds.
    pub auth_window: Duration,
}

impl Default for RateLimits {
    fn default() -> Self {
        RateLimits {
            auth_limit: DEFAULT_AUTH_LIMIT,
            auth_window: DEFAULT_RATE_WINDOW,
        }
    }
}

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

type RateKey = (RateClass, ClientKey);

/// "No node" in the intrusive list.
const NIL: usize = usize::MAX;

/// One live window in a shard's list.
struct Node {
    key: RateKey,
    /// When the window began (monotonic).
    start: Duration,
    count: u32,
    prev: usize,
    next: usize,
}

/// One shard: a map from key to a slot, and the slots in a doubly linked list ordered by window
/// start (oldest at the head). A window that restarts moves to the tail, so the head is always
/// the oldest window and therefore the first to be expired. Every operation is O(1): nothing
/// here scans the table.
struct Shard {
    capacity: usize,
    map: HashMap<RateKey, usize>,
    nodes: Vec<Node>,
    free: Vec<usize>,
    head: usize,
    tail: usize,
}

impl Shard {
    fn new(capacity: usize) -> Self {
        Shard {
            capacity,
            map: HashMap::new(),
            nodes: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
        }
    }

    fn unlink(&mut self, i: usize) {
        let (prev, next) = (self.nodes[i].prev, self.nodes[i].next);
        match prev {
            NIL => self.head = next,
            p => self.nodes[p].next = next,
        }
        match next {
            NIL => self.tail = prev,
            n => self.nodes[n].prev = prev,
        }
        self.nodes[i].prev = NIL;
        self.nodes[i].next = NIL;
    }

    fn push_back(&mut self, i: usize) {
        self.nodes[i].prev = self.tail;
        self.nodes[i].next = NIL;
        match self.tail {
            NIL => self.head = i,
            t => self.nodes[t].next = i,
        }
        self.tail = i;
    }

    /// Drops the oldest window (the head), if any.
    fn evict_oldest(&mut self) {
        let i = self.head;
        if i == NIL {
            return;
        }
        self.unlink(i);
        let key = self.nodes[i].key;
        self.map.remove(&key);
        self.free.push(i);
    }

    /// Admits a new key with a window starting `now` and one request counted. Only when the
    /// shard is full is the oldest window evicted: below capacity nothing ever is.
    fn admit(&mut self, key: RateKey, now: Duration) {
        if self.map.len() >= self.capacity {
            self.evict_oldest();
        }
        let node = Node {
            key,
            start: now,
            count: 1,
            prev: NIL,
            next: NIL,
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = node;
                i
            }
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            }
        };
        self.map.insert(key, i);
        self.push_back(i);
    }
}

/// A fixed-window limiter keyed by (rate class, client address), timed by the server's injected
/// clock (monotonic, never the wall clock). One counter per class and client address, shared by
/// every public group of that class.
///
/// **Structure.** The table is sharded: `RATE_SHARDS` independent mutex-guarded maps, selected by
/// a keyed hash of the key (the hash key is random per limiter, so which shard an address lands
/// in cannot be steered). Each shard keeps its windows in a linked list ordered by window start.
/// A request takes one shard's lock, does a map lookup and at most one list move, insert or
/// eviction: O(1), no scan of the table, no global lock. **A full shard evicts its oldest
/// window to admit a new key** (an expired window goes first, being the oldest); below capacity
/// nothing is evicted. A flood of distinct addresses therefore displaces other clients'
/// counters only at the rate of its own requests, and never blocks a new client.
///
/// **Residual behaviour.** An attacker that cycles more than a shard's capacity of distinct
/// addresses inside one window can push a limited client's counter out and so reset it. The
/// capacity is therefore a sized, documented constant ([`RATE_TABLE_CAPACITY`], 65,536 windows
/// in total) and the keys are IPv6 /64s, so the attacker needs that many /64s (an IPv6 /48 is
/// exactly that many) and must sustain the rate for the whole window.
pub struct RateLimiter {
    clock: Arc<dyn Clock>,
    limits: RateLimits,
    hasher: RandomState,
    shards: Vec<Mutex<Shard>>,
}

impl RateLimiter {
    /// A limiter with the default limits.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_limits(clock, RateLimits::default())
    }

    /// A limiter with `limits` and the standard table capacity.
    pub fn with_limits(clock: Arc<dyn Clock>, limits: RateLimits) -> Self {
        Self::build(
            clock,
            limits,
            RATE_SHARDS,
            RATE_TABLE_CAPACITY / RATE_SHARDS,
        )
    }

    /// A limiter with a small table, for tests of the eviction rule: `shards` tables of
    /// `capacity_per_shard` windows each.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_capacity(
        clock: Arc<dyn Clock>,
        limits: RateLimits,
        shards: usize,
        capacity_per_shard: usize,
    ) -> Self {
        Self::build(clock, limits, shards.max(1), capacity_per_shard.max(1))
    }

    fn build(clock: Arc<dyn Clock>, limits: RateLimits, shards: usize, capacity: usize) -> Self {
        RateLimiter {
            clock,
            limits,
            hasher: RandomState::new(),
            shards: (0..shards)
                .map(|_| Mutex::new(Shard::new(capacity)))
                .collect(),
        }
    }

    fn limit(&self, class: RateClass) -> u32 {
        match class {
            RateClass::Auth => self.limits.auth_limit,
        }
    }

    fn window(&self, class: RateClass) -> Duration {
        match class {
            RateClass::Auth => self.limits.auth_window,
        }
    }

    /// How many windows are held now (all shards), for tests.
    #[cfg(any(test, feature = "testing"))]
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(|p| p.into_inner()).map.len())
            .sum()
    }

    /// Whether no window is held, for tests.
    #[cfg(any(test, feature = "testing"))]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Counts one request of `class` from `peer`; `Err(seconds)` is the wait until the window
    /// ends. O(1): one shard's lock, one map lookup, at most one list operation.
    pub fn check(&self, class: RateClass, peer: Option<IpAddr>) -> Result<(), u64> {
        let now = self.clock.monotonic();
        let key: RateKey = (class, ClientKey::of(peer));
        let (limit, window) = (self.limit(class), self.window(class));
        let shard = self.hasher.hash_one(key) as usize % self.shards.len();
        let mut shard = self.shards[shard].lock().unwrap_or_else(|p| p.into_inner());
        let wait = |start: Duration| {
            (start + window)
                .saturating_sub(now)
                .as_secs()
                .saturating_add(1)
        };
        let Some(&i) = shard.map.get(&key) else {
            shard.admit(key, now);
            return Ok(());
        };
        let (start, count) = (shard.nodes[i].start, shard.nodes[i].count);
        if now.saturating_sub(start) >= window {
            // The window ended: a fresh one begins now, and is the newest.
            shard.nodes[i].start = now;
            shard.nodes[i].count = 1;
            shard.unlink(i);
            shard.push_back(i);
            return Ok(());
        }
        if count >= limit {
            return Err(wait(start));
        }
        shard.nodes[i].count = count + 1;
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
///
/// The request fingerprint covers the method, the path **and the raw query** as received
/// (`path?query`, no reordering or normalisation: a different order is a different request,
/// which is the safe direction). A request that was in flight at deploy time under the old
/// fingerprint (path only) answers `409 api.idempotency_conflict` to its own retry once, since
/// a key lives 24 hours; that is accepted.
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
    // The query is part of what was asked: `?dry_run=true` and `?dry_run=false` are different
    // requests and must not share a fingerprint (and so a stored answer).
    let path = match parts.uri.query() {
        Some(query) => format!("{}?{query}", parts.uri.path()),
        None => parts.uri.path().to_owned(),
    };
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
    // bogus token would otherwise write one durable row per unauthenticated request. So is
    // `403`: a valid session without the right (or a token the service rejects as forbidden)
    // would otherwise replay a refusal for 24 hours to a retry made after the right was
    // granted, and write one durable row per refused request. A `404` or a `409` remains stored
    // and replayed: those describe the state of the data, not the caller's standing.
    let release = status.is_server_error()
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
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
#[allow(clippy::unwrap_used)]
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

    /// An auth port that says every credential is for one audience and counts its calls.
    struct OneAudience(Audience, std::sync::atomic::AtomicUsize);

    impl AuthService for OneAudience {
        fn login<'a>(
            &'a self,
            _: &'a RequestCtx,
            _: &'a crate::api::service::LoginBody,
        ) -> crate::api::service::BoxFuture<'a, Result<crate::api::service::Session, ApiError>>
        {
            Box::pin(async { Err(ApiError::from_code(Code::HttpInternal)) })
        }
        fn logout<'a>(
            &'a self,
            _: &'a RequestCtx,
        ) -> crate::api::service::BoxFuture<'a, Result<(), ApiError>> {
            Box::pin(async { Ok(()) })
        }
        fn session<'a>(
            &'a self,
            _: &'a RequestCtx,
        ) -> crate::api::service::BoxFuture<'a, Result<crate::api::service::SessionInfo, ApiError>>
        {
            Box::pin(async { Err(ApiError::from_code(Code::HttpInternal)) })
        }
        fn audience_of<'a>(
            &'a self,
            _: &'a RequestCtx,
        ) -> crate::api::service::BoxFuture<'a, Result<Audience, ApiError>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let audience = self.0;
            Box::pin(async move { Ok(audience) })
        }
    }

    fn gate(strict: bool) -> (AuthGate, Arc<OneAudience>) {
        let auth = Arc::new(OneAudience(
            Audience::Staff,
            std::sync::atomic::AtomicUsize::new(0),
        ));
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        (AuthGate::new(auth.clone(), clock, strict), auth)
    }

    /// A router with one route at `path` (answering `method`) behind the bearer layer, and a
    /// flag the handler sets.
    fn guarded(
        gate: AuthGate,
        path: &'static str,
        post: bool,
        reached: Arc<std::sync::atomic::AtomicBool>,
    ) -> axum::Router {
        let handler = move || {
            let reached = reached.clone();
            async move {
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
                "handler"
            }
        };
        let route = if post {
            axum::routing::post(handler)
        } else {
            axum::routing::get(handler)
        };
        axum::Router::new()
            .route(path, route)
            .layer(axum::middleware::from_fn_with_state(gate, auth_middleware))
    }

    async fn status_of(app: &axum::Router, method: &str, path: &str) -> StatusCode {
        use tower::ServiceExt as _;
        let request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer test-token")
            .body(axum::body::Body::empty())
            .unwrap_or_else(|error| panic!("request: {error}"));
        match app.clone().oneshot(request).await {
            Ok(response) => response.status(),
            Err(never) => match never {},
        }
    }

    #[tokio::test]
    async fn a_secured_route_without_a_mark_is_a_500_and_runs_no_handler() {
        let (strict, auth) = gate(true);
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let app = guarded(strict, "/api/v1/not-in-the-table", false, reached.clone());
        assert_eq!(
            status_of(&app, "GET", "/api/v1/not-in-the-table").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(!reached.load(std::sync::atomic::Ordering::SeqCst));
        // The audience was never even asked for: the wiring error comes first.
        assert_eq!(auth.1.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_lenient_gate_lets_an_unmarked_test_route_through_on_bearer_presence() {
        let (lenient, _) = gate(false);
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let app = guarded(lenient, "/api/v1/not-in-the-table", false, reached.clone());
        assert_eq!(
            status_of(&app, "GET", "/api/v1/not-in-the-table").await,
            StatusCode::OK
        );
        assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_method_the_table_does_not_list_on_a_known_path_is_the_routers_405() {
        let (strict, _) = gate(true);
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The table lists GET for this path; the route here is POST only, so a GET has no
        // handler and no mark: the router answers 405 and nothing runs.
        let app = guarded(strict, "/api/v1/auth/session", true, reached.clone());
        assert_eq!(
            status_of(&app, "GET", "/api/v1/auth/session").await,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert!(!reached.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn the_refusal_log_is_bounded_to_one_line_per_key_per_window() {
        let auth = Arc::new(OneAudience(
            Audience::Staff,
            std::sync::atomic::AtomicUsize::new(0),
        ));
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let gate = AuthGate::new(auth, clock.clone(), true);
        assert!(gate.may_log(Audience::Consumer, "auth_session"));
        assert!(!gate.may_log(Audience::Consumer, "auth_session"));
        // Another key is its own bucket.
        assert!(gate.may_log(Audience::Partner, "auth_session"));
        assert!(gate.may_log(Audience::Consumer, "auth_logout"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod rate_limiter_tests {
    use super::*;
    use spot_core::ManualClock;
    use std::net::Ipv4Addr;
    use time::OffsetDateTime;

    fn addr(n: u32) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n)))
    }

    fn limiter(limit: u32, shards: usize, capacity: usize) -> (RateLimiter, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let limits = RateLimits {
            auth_limit: limit,
            auth_window: Duration::from_secs(60),
        };
        (
            RateLimiter::with_capacity(clock.clone(), limits, shards, capacity),
            clock,
        )
    }

    #[test]
    fn below_capacity_nothing_is_evicted() {
        let (l, _) = limiter(1, 1, 4);
        for n in 0..4 {
            assert!(l.check(RateClass::Auth, addr(n)).is_ok());
        }
        assert_eq!(l.len(), 4);
        // Every one of the four is still counted: a second request is limited.
        for n in 0..4 {
            assert!(l.check(RateClass::Auth, addr(n)).is_err(), "{n}");
        }
    }

    #[test]
    fn a_full_table_admits_a_new_key_by_evicting_the_oldest_window() {
        let (l, clock) = limiter(1, 1, 4);
        for n in 0..4 {
            clock.advance(Duration::from_secs(1));
            assert!(l.check(RateClass::Auth, addr(n)).is_ok());
        }
        // A new address is admitted, never refused, and counted from one.
        assert!(l.check(RateClass::Auth, addr(100)).is_ok());
        assert_eq!(l.len(), 4);
        assert!(
            l.check(RateClass::Auth, addr(100)).is_err(),
            "counted from 1"
        );
        // The oldest (address 0) lost its counter and starts afresh; the others kept theirs.
        assert!(l.check(RateClass::Auth, addr(0)).is_ok());
        assert!(l.check(RateClass::Auth, addr(3)).is_err());
    }

    #[test]
    fn a_window_that_restarted_is_the_newest_not_the_oldest() {
        let (l, clock) = limiter(1, 1, 2);
        assert!(l.check(RateClass::Auth, addr(1)).is_ok());
        clock.advance(Duration::from_secs(30));
        assert!(l.check(RateClass::Auth, addr(2)).is_ok());
        clock.advance(Duration::from_secs(31));
        // Address 1's window ended: it restarts and moves behind address 2.
        assert!(l.check(RateClass::Auth, addr(1)).is_ok());
        // Admitting a third evicts the oldest window, which is address 2's.
        assert!(l.check(RateClass::Auth, addr(3)).is_ok());
        assert!(
            l.check(RateClass::Auth, addr(1)).is_err(),
            "address 1 kept its counter"
        );
        assert!(
            l.check(RateClass::Auth, addr(2)).is_ok(),
            "address 2 was evicted"
        );
    }

    #[test]
    fn a_limited_client_stays_limited_while_its_entry_survives() {
        let (l, _) = limiter(2, 1, 8);
        assert!(l.check(RateClass::Auth, addr(1)).is_ok());
        assert!(l.check(RateClass::Auth, addr(1)).is_ok());
        let wait = l.check(RateClass::Auth, addr(1)).unwrap_err();
        assert!((1..=61).contains(&wait), "{wait}");
        // Other clients flooding below capacity do not free it.
        for n in 10..13 {
            assert!(l.check(RateClass::Auth, addr(n)).is_ok());
        }
        assert!(l.check(RateClass::Auth, addr(1)).is_err());
    }

    #[test]
    fn ipv6_is_reduced_to_its_64_and_unknown_shares_one_bucket() {
        let (l, _) = limiter(1, 1, 8);
        let a: IpAddr = "2001:db8:1:1::5".parse().unwrap();
        let b: IpAddr = "2001:db8:1:1::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        assert!(l.check(RateClass::Auth, Some(a)).is_ok());
        assert!(l.check(RateClass::Auth, Some(b)).is_err());
        assert!(l.check(RateClass::Auth, Some(c)).is_ok());
        let mapped: IpAddr = "::ffff:10.0.0.200".parse().unwrap();
        assert!(l.check(RateClass::Auth, Some(mapped)).is_ok());
        assert!(
            l.check(RateClass::Auth, addr(200)).is_err(),
            "mapped is the IPv4 key"
        );
        assert!(l.check(RateClass::Auth, None).is_ok());
        assert!(l.check(RateClass::Auth, None).is_err());
    }

    #[test]
    fn many_distinct_keys_stay_within_capacity_and_do_not_go_quadratic() {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let l = RateLimiter::new(clock);
        let started = std::time::Instant::now();
        for n in 0..200_000u32 {
            assert!(l.check(RateClass::Auth, addr(n)).is_ok(), "{n}");
            if n % 1000 == 0 {
                assert!(l.len() <= RATE_TABLE_CAPACITY);
            }
        }
        assert!(l.len() <= RATE_TABLE_CAPACITY);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "200,000 keys took {elapsed:?}"
        );
    }

    #[test]
    fn concurrent_checks_never_exceed_the_limit_of_one_key_in_a_window() {
        let (l, _) = limiter(50, 4, 1024);
        let l = Arc::new(l);
        let admitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for t in 0..8u32 {
            let (l, admitted) = (l.clone(), admitted.clone());
            handles.push(std::thread::spawn(move || {
                for i in 0..100u32 {
                    // Same key for everyone, plus a distinct key per thread.
                    if l.check(RateClass::Auth, addr(1)).is_ok() {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    let _ = l.check(RateClass::Auth, addr(1000 + t * 1000 + i));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(admitted.load(std::sync::atomic::Ordering::SeqCst), 50);
    }
}
