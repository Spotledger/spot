//! The state machine: `Core::handle(Input) -> Vec<Output>`.

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value, json};

use crate::cache::{Cache, CacheKey, Entry};
use crate::path;
use crate::problem::Problem;
use crate::token::SessionToken;
use crate::types::{Event, Header, HttpRequest, Input, Output, Reply, Request};

/// A request is sent at most this many times (the first send plus two retries).
pub const MAX_SENDS: u8 = 3;
/// A `Retry-After` above this many seconds is cut to it.
const MAX_RETRY_AFTER_S: u64 = 60;
/// The longest backoff when the server named no `Retry-After`, in seconds.
const MAX_BACKOFF_S: u64 = 30;

/// What a request is, which decides how its answer is handled.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    SignIn,
    SignOut,
    Call,
    Get(CacheKey),
    List(CacheKey),
}

/// A request on its way, kept until it settles.
pub(crate) struct Pending {
    pub(crate) req_id: u64,
    pub(crate) op: Op,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Option<String>,
    pub(crate) if_match: Option<u64>,
    pub(crate) idempotency_key: Option<String>,
    pub(crate) sends: u8,
    pub(crate) reauthed: bool,
    /// Read back from the host's storage (feature `storage`): it was admitted to a write by the
    /// rule of the session that stored it and stays until the core sees metadata with a secret.
    /// Never set for a call created in this session.
    #[cfg(feature = "storage")]
    pub(crate) restored: bool,
}

impl Pending {
    fn mutating(&self) -> bool {
        matches!(self.op, Op::Call) && self.method != "GET"
    }

    /// True when sending it again after a lost reply cannot do harm.
    fn safe_to_resend(&self) -> bool {
        !self.mutating() && !matches!(self.op, Op::SignIn)
            || self.idempotency_key.is_some()
            || self.if_match.is_some()
    }
}

/// The client core. It owns no clock, socket, file or random source: everything it knows
/// arrives as an [`Input`] and everything it wants leaves as an [`Output`].
#[derive(Default)]
pub struct Core {
    pub(crate) now_ms: u64,
    next_http_id: u64,
    pub(crate) session: Option<SessionToken>,
    pub(crate) inflight: BTreeMap<u64, Pending>,
    pub(crate) blocked: VecDeque<Pending>,
    pub(crate) retrying: Vec<(u64, Pending)>,
    pub(crate) held: Vec<Pending>,
    pub(crate) cache: Cache,
    /// The DocTypes whose metadata was seen to hold no `secret` field; cleared with the cache.
    #[cfg(feature = "storage")]
    pub(crate) meta: crate::meta_seen::MetaSeen,
    /// The user the cache and the held requests belong to; a different user signing in
    /// starts from an empty cache so one user's read can never answer another's.
    pub(crate) cache_user: Option<String>,
    pub(crate) rooms: crate::realtime::Rooms,
    #[cfg(feature = "storage")]
    pub(crate) persisted: Option<String>,
    /// The site the host named at start; with the user id it owns the stored queue.
    #[cfg(feature = "storage")]
    pub(crate) site: String,
    /// The queue read from storage, held apart until a sign-in has established the user.
    #[cfg(feature = "storage")]
    pub(crate) restored: Option<crate::storage::Restored>,
}

impl Core {
    /// A core with no session, no queue and an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one input and returns what the host must do. Never panics; an input that makes
    /// no sense in the current state (a reply to a request that is not in flight) returns
    /// no outputs.
    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::Request(request) => self.on_request(request, &mut out),
            Input::TransportReply { id, reply } => self.on_reply(id, &reply, &mut out),
            Input::TransportError { id } => self.on_transport_error(id, &mut out),
            Input::Tick { now_ms } => self.on_tick(now_ms, &mut out),
            #[cfg(feature = "storage")]
            Input::StorageReply { id, value } => self.on_storage_reply(id, value, &mut out),
        }
        #[cfg(feature = "storage")]
        self.persist(&mut out);
        out
    }

    /// True when a session token is held.
    pub fn is_signed_in(&self) -> bool {
        self.session.is_some()
    }

    fn on_request(&mut self, request: Request, out: &mut Vec<Output>) {
        match request {
            Request::SignIn {
                req_id,
                user,
                password,
                totp,
            } => {
                let mut body = json!({ "user": user, "password": password });
                if let (Some(totp), Some(map)) = (totp, body.as_object_mut()) {
                    map.insert("totp".to_owned(), Value::String(totp));
                }
                let pending =
                    Self::pending(req_id, Op::SignIn, "POST", "/api/v1/auth/login".to_owned());
                self.dispatch(
                    Pending {
                        body: Some(body.to_string()),
                        ..pending
                    },
                    out,
                );
            }
            Request::SignOut { req_id } => {
                let pending = Self::pending(
                    req_id,
                    Op::SignOut,
                    "POST",
                    "/api/v1/auth/logout".to_owned(),
                );
                self.dispatch(pending, out);
            }
            Request::Call {
                req_id,
                method,
                path,
                body,
                if_match,
                idempotency_key,
            } => {
                let method = method.to_ascii_uppercase();
                if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE")
                    || !path.starts_with('/')
                {
                    out.push(Self::local_failure(
                        req_id,
                        "client.bad_request",
                        "The method or path is not valid.",
                    ));
                    return;
                }
                let pending = Pending {
                    body,
                    if_match,
                    idempotency_key,
                    ..Self::pending(req_id, Op::Call, &method, path)
                };
                self.dispatch_authed(pending, out);
            }
            Request::ResourceGet {
                req_id,
                doctype,
                id,
            } => {
                let key = CacheKey::Doc {
                    doctype: doctype.clone(),
                    id: id.clone(),
                };
                let path = format!(
                    "/api/v1/resource/{}/{}",
                    path::encode(&doctype),
                    path::encode(&id)
                );
                self.dispatch_authed(Self::pending(req_id, Op::Get(key), "GET", path), out);
            }
            Request::ResourceList {
                req_id,
                doctype,
                query,
            } => {
                let query = query.unwrap_or_default();
                let key = CacheKey::List {
                    doctype: doctype.clone(),
                    query: query.clone(),
                };
                let mut path = format!("/api/v1/resource/{}", path::encode(&doctype));
                if !query.is_empty() {
                    path.push('?');
                    path.push_str(&query);
                }
                self.dispatch_authed(Self::pending(req_id, Op::List(key), "GET", path), out);
            }
            Request::Subscribe { room } => self.subscribe(room, out),
            Request::Realtime(message) => self.on_realtime(message, out),
        }
    }

    fn pending(req_id: u64, op: Op, method: &str, path: String) -> Pending {
        Pending {
            req_id,
            op,
            method: method.to_owned(),
            path,
            body: None,
            if_match: None,
            idempotency_key: None,
            sends: 0,
            reauthed: false,
            #[cfg(feature = "storage")]
            restored: false,
        }
    }

    pub(crate) fn local_failure(req_id: u64, code: &str, message: &str) -> Output {
        Output::Emit(Event::Failed {
            req_id,
            status: 0,
            problem: Problem::local(code, message, false),
        })
    }

    fn dispatch_authed(&mut self, pending: Pending, out: &mut Vec<Output>) {
        if self.session.is_none() {
            out.push(Self::local_failure(
                pending.req_id,
                "auth.not_signed_in",
                "Sign in first.",
            ));
            return;
        }
        self.dispatch(pending, out);
    }

    /// Sends a request, or parks it behind an earlier state-changing request to the same path.
    pub(crate) fn dispatch(&mut self, pending: Pending, out: &mut Vec<Output>) {
        if pending.mutating() && self.path_busy(&pending.path) {
            self.blocked.push_back(pending);
        } else {
            self.send(pending, out);
        }
    }

    fn path_busy(&self, path: &str) -> bool {
        self.inflight
            .values()
            .any(|p| p.mutating() && p.path == path)
            || self
                .retrying
                .iter()
                .any(|(_, p)| p.mutating() && p.path == path)
    }

    fn release_blocked(&mut self, out: &mut Vec<Output>) {
        let mut i = 0;
        while i < self.blocked.len() {
            let free = self
                .blocked
                .get(i)
                .is_some_and(|p| !self.path_busy(&p.path));
            match free.then(|| self.blocked.remove(i)).flatten() {
                Some(pending) => self.send(pending, out),
                None => i += 1,
            }
        }
    }

    fn send(&mut self, mut pending: Pending, out: &mut Vec<Output>) {
        pending.sends = pending.sends.saturating_add(1);
        self.next_http_id = self.next_http_id.saturating_add(1);
        let id = self.next_http_id;
        let mut headers: Vec<Header> = vec![("Accept".to_owned(), "application/json".to_owned())];
        if !matches!(pending.op, Op::SignIn)
            && let Some(token) = &self.session
        {
            headers.push(("Authorization".to_owned(), token.bearer_header()));
        }
        if pending.body.is_some() {
            headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        if let Some(version) = pending.if_match {
            headers.push(("If-Match".to_owned(), format!("\"{version}\"")));
        }
        if let Some(key) = &pending.idempotency_key {
            headers.push(("Idempotency-Key".to_owned(), key.clone()));
        }
        if let Op::Get(key) | Op::List(key) = &pending.op
            && let Some(entry) = self.cache.get(key)
        {
            headers.push(("If-None-Match".to_owned(), entry.etag.clone()));
        }
        let request = HttpRequest {
            method: pending.method.clone(),
            path: pending.path.clone(),
            headers,
            body: pending.body.clone(),
        };
        self.inflight.insert(id, pending);
        out.push(Output::SendHttp { id, request });
    }

    fn on_reply(&mut self, id: u64, reply: &Reply, out: &mut Vec<Output>) {
        let Some(pending) = self.inflight.remove(&id) else {
            return;
        };
        let ok = (200..300).contains(&reply.status)
            || (reply.status == 304 && matches!(pending.op, Op::Get(_) | Op::List(_)));
        if ok {
            self.succeed(pending, reply, out);
        } else {
            let problem = Problem::parse(reply.status, &reply.body);
            self.fail(pending, reply, problem, out);
        }
        self.release_blocked(out);
    }

    fn succeed(&mut self, pending: Pending, reply: &Reply, out: &mut Vec<Output>) {
        let req_id = pending.req_id;
        match &pending.op {
            Op::SignIn => self.finish_sign_in(req_id, reply, out),
            Op::SignOut => self.finish_sign_out(req_id, out),
            Op::Call => {
                #[cfg(feature = "storage")]
                self.meta
                    .learn(&pending.method, &pending.path, reply.status, &reply.body);
                if pending.mutating()
                    && let Some((doctype, id)) = path::resource_of(&pending.path)
                {
                    self.cache.invalidate_doc(&doctype, &id);
                }
                let replayed = reply
                    .header("Idempotency-Replayed")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                out.push(Output::Emit(Event::Done {
                    req_id,
                    status: reply.status,
                    body: reply.body.clone(),
                    replayed,
                }));
            }
            Op::Get(key) | Op::List(key) => {
                if reply.status == 304 {
                    match self.cache.get(key) {
                        Some(entry) => out.push(Output::Emit(Event::Resource {
                            req_id,
                            body: entry.body.clone(),
                            from_cache: true,
                        })),
                        None => out.push(Self::local_failure(
                            req_id,
                            "client.cache_miss",
                            "A 304 arrived for an entry the cache does not hold.",
                        )),
                    }
                } else {
                    if let Some(etag) = reply.header("ETag") {
                        self.cache.put(
                            key.clone(),
                            Entry {
                                etag: etag.to_owned(),
                                body: reply.body.clone(),
                            },
                        );
                    }
                    out.push(Output::Emit(Event::Resource {
                        req_id,
                        body: reply.body.clone(),
                        from_cache: false,
                    }));
                }
            }
        }
    }

    fn finish_sign_in(&mut self, req_id: u64, reply: &Reply, out: &mut Vec<Output>) {
        let parsed: Option<Value> = serde_json::from_str(&reply.body).ok();
        let data = parsed.as_ref().and_then(|v| v.get("data"));
        let token = data.and_then(|d| d.get("token")).and_then(Value::as_str);
        let Some(token) = token else {
            out.push(Self::local_failure(
                req_id,
                "client.unparseable_response",
                "The sign-in answer carried no token.",
            ));
            return;
        };
        let text =
            |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or_default().to_owned();
        let expires_at = text(data.and_then(|d| d.get("expires_at")));
        let user_id = text(data.and_then(|d| d.get("user")).and_then(|u| u.get("id")));
        self.session = Some(SessionToken::new(token.to_owned()));
        // An unknown user id never matches, so it never reuses a cache.
        let same_user = !user_id.is_empty() && self.cache_user.as_deref() == Some(user_id.as_str());
        if !same_user {
            self.cache.clear();
            #[cfg(feature = "storage")]
            {
                self.meta.clear();
                // A call restored for the previous user is no longer the host's to keep.
                self.unmark_all_restored();
            }
            // Requests held for the previous user are not replayed as someone else. A queue
            // restored from storage is not in `held`: it is judged by its stored owner below.
            if self.cache_user.is_some() {
                for dropped in std::mem::take(&mut self.held) {
                    out.push(Self::local_failure(
                        dropped.req_id,
                        "auth.session_changed",
                        "A different user signed in; the request was not sent.",
                    ));
                }
            }
            self.cache_user = (!user_id.is_empty()).then(|| user_id.clone());
        }
        #[cfg(feature = "storage")]
        self.settle_restored(Some(&user_id), out);
        out.push(Output::Emit(Event::SignedIn {
            req_id,
            user_id,
            expires_at,
        }));
        // The one retry after re-auth: each held request goes out again, marked so that a
        // second 401 fails it instead of holding it again.
        for mut held in std::mem::take(&mut self.held) {
            held.reauthed = true;
            held.sends = 0;
            self.dispatch(held, out);
        }
    }

    fn finish_sign_out(&mut self, req_id: u64, out: &mut Vec<Output>) {
        self.session = None;
        self.inflight.clear();
        self.blocked.clear();
        self.retrying.clear();
        self.held.clear();
        #[cfg(feature = "storage")]
        {
            self.restored = None;
        }
        self.cache.clear();
        #[cfg(feature = "storage")]
        self.meta.clear();
        out.push(Output::Emit(Event::SignedOut { req_id }));
    }

    fn fail(&mut self, pending: Pending, reply: &Reply, problem: Problem, out: &mut Vec<Output>) {
        let req_id = pending.req_id;
        let status = reply.status;
        if status == 401 {
            match pending.op {
                Op::SignOut => return self.finish_sign_out(req_id, out),
                Op::SignIn => {}
                _ if !pending.reauthed => {
                    self.session = None;
                    self.held.push(pending);
                    out.push(Output::Emit(Event::ReauthRequired { req_id }));
                    return;
                }
                _ => {}
            }
        }
        let retryable =
            matches!(status, 429 | 503) || (problem.retryable && status != 401 && status != 412);
        if retryable && pending.sends < MAX_SENDS {
            let retry_after = reply
                .header("Retry-After")
                .and_then(|v| v.trim().parse::<u64>().ok());
            self.schedule_retry(pending, retry_after, out);
        } else if status == 412 {
            out.push(Output::Emit(Event::Conflict {
                req_id,
                current_version: problem.current_version,
            }));
        } else {
            out.push(Output::Emit(Event::Failed {
                req_id,
                status,
                problem,
            }));
        }
    }

    fn on_transport_error(&mut self, id: u64, out: &mut Vec<Output>) {
        let Some(pending) = self.inflight.remove(&id) else {
            return;
        };
        if pending.safe_to_resend() && pending.sends < MAX_SENDS {
            self.schedule_retry(pending, None, out);
        } else {
            let problem = Problem::local("client.network", "The request got no reply.", true);
            out.push(Output::Emit(Event::Failed {
                req_id: pending.req_id,
                status: 0,
                problem,
            }));
        }
        self.release_blocked(out);
    }

    fn schedule_retry(
        &mut self,
        pending: Pending,
        retry_after: Option<u64>,
        out: &mut Vec<Output>,
    ) {
        let backoff = 1_u64
            .checked_shl(u32::from(pending.sends.saturating_sub(1)))
            .unwrap_or(MAX_BACKOFF_S)
            .min(MAX_BACKOFF_S);
        let delay_s = retry_after.map_or(backoff, |s| s.min(MAX_RETRY_AFTER_S));
        let due = self.now_ms.saturating_add(delay_s.saturating_mul(1000));
        self.retrying.push((due, pending));
        out.push(Output::SetTimer { at_ms: due });
    }

    fn on_tick(&mut self, now_ms: u64, out: &mut Vec<Output>) {
        self.now_ms = self.now_ms.max(now_ms);
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.retrying)
            .into_iter()
            .partition(|(at, _)| *at <= self.now_ms);
        self.retrying = later;
        let mut due = due;
        due.sort_by_key(|(at, _)| *at);
        for (_, pending) in due {
            self.send(pending, out);
        }
    }
}
