//! The boundary: every value that crosses it is plain data.

use serde::{Deserialize, Serialize};

use crate::problem::Problem;

/// One HTTP header, name and value.
pub type Header = (String, String);

/// What the host sends into the core.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    /// A request from the application.
    Request(Request),
    /// The answer to an earlier `SendHttp`.
    TransportReply {
        /// The id of the `SendHttp` it answers.
        id: u64,
        /// The reply.
        reply: Reply,
    },
    /// The transport failed before any reply (network down, timeout).
    TransportError {
        /// The id of the `SendHttp` it answers.
        id: u64,
    },
    /// The host's clock, in milliseconds on any monotonic scale it chooses. The core has
    /// no other clock: time only moves when a `Tick` says so.
    Tick {
        /// The host's current time.
        now_ms: u64,
    },
    /// The answer to a `StorageRead` (only with the `storage` feature).
    #[cfg(feature = "storage")]
    StorageReply {
        /// The id of the `StorageRead` it answers.
        id: u64,
        /// The stored text, or `None` when nothing was stored.
        value: Option<String>,
    },
}

/// What the application asks of the core.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Sign in. The password is used for the one request and never kept.
    SignIn {
        /// The application's id for this request.
        req_id: u64,
        /// The user name or email.
        user: String,
        /// The password.
        password: String,
        /// The second factor, on the repeat after `auth.totp_required`.
        #[serde(default)]
        totp: Option<String>,
    },
    /// Sign out: ends the session and drops the queue and the cache.
    SignOut {
        /// The application's id for this request.
        req_id: u64,
    },
    /// Any call. State-changing calls to one path are sent one at a time, in order.
    Call {
        /// The application's id for this request.
        req_id: u64,
        /// `GET`, `POST`, `PATCH` or `DELETE`.
        method: String,
        /// The path, already percent-encoded, starting with `/`.
        path: String,
        /// The JSON body, if any.
        #[serde(default)]
        body: Option<String>,
        /// The document version for `If-Match`.
        #[serde(default)]
        if_match: Option<u64>,
        /// The `Idempotency-Key`, minted by the caller at enqueue and kept across retries.
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    /// `resource_get`: one document, cached by its `ETag`.
    ResourceGet {
        /// The application's id for this request.
        req_id: u64,
        /// The DocType name.
        doctype: String,
        /// The document id.
        id: String,
    },
    /// `resource_list`: a page of documents, cached by its `ETag`.
    ResourceList {
        /// The application's id for this request.
        req_id: u64,
        /// The DocType name.
        doctype: String,
        /// The query string without `?`, already percent-encoded.
        #[serde(default)]
        query: Option<String>,
    },
    /// Start listening to a realtime room.
    Subscribe {
        /// The room name.
        room: String,
    },
    /// A message from the realtime transport.
    Realtime(RealtimeMsg),
}

/// A realtime message, reduced to what the core needs (`contracts/native-api/realtime.md`).
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RealtimeMsg {
    /// `joined`, with its `outcome`: `joined`, `replayed` or `resync_required`.
    Joined {
        /// The room.
        room: String,
        /// The outcome.
        outcome: String,
    },
    /// An `event` frame.
    Event {
        /// The room.
        room: String,
        /// The room's sequence number.
        seq: u64,
        /// `doc_updated` and the other event kinds.
        event: String,
        /// The payload object.
        payload: serde_json::Value,
    },
    /// The socket closed: every room is rejoined with its resume position.
    Disconnected,
}

/// An HTTP reply as the host saw it.
#[derive(Debug, Clone, Deserialize)]
pub struct Reply {
    /// The status code.
    pub status: u16,
    /// The headers.
    #[serde(default)]
    pub headers: Vec<Header>,
    /// The body text.
    #[serde(default)]
    pub body: String,
}

impl Reply {
    /// The first header named `name`, ignoring case.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An HTTP request for the host to send.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct HttpRequest {
    /// The method.
    pub method: String,
    /// The path and query.
    pub path: String,
    /// The headers; `Authorization` carries the session token.
    pub headers: Vec<Header>,
    /// The body, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let hidden = k.eq_ignore_ascii_case("authorization");
                (k.as_str(), if hidden { "<redacted>" } else { v.as_str() })
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers", &headers)
            .field("body", &self.body.as_ref().map(|_| "<body>"))
            .finish()
    }
}

/// What the core asks the host to do.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Output {
    /// Send this request and answer with `TransportReply` or `TransportError`.
    SendHttp {
        /// The id to answer with.
        id: u64,
        /// The request.
        request: HttpRequest,
    },
    /// Read a stored value and answer with `StorageReply` (only with the `storage` feature).
    #[cfg(feature = "storage")]
    StorageRead {
        /// The id to answer with.
        id: u64,
        /// The key.
        key: String,
    },
    /// Store a value, or remove the key when `value` is `None` (only with `storage`).
    #[cfg(feature = "storage")]
    StorageWrite {
        /// The key.
        key: String,
        /// The text to store.
        value: Option<String>,
    },
    /// Wake the core with a `Tick` at or after `at_ms`.
    SetTimer {
        /// The time to wake at, on the host's `Tick` scale.
        at_ms: u64,
    },
    /// Tell the application something.
    Emit(Event),
}

/// Why a restored queue was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscardReason {
    /// A queue written for another user or another site.
    OwnerChanged,
    /// The old format: a bare list of calls with no owner.
    Untagged,
    /// Not a queue this version can read.
    Unreadable,
}

/// What the application is told.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Signed in.
    SignedIn {
        /// The request.
        req_id: u64,
        /// The user id.
        user_id: String,
        /// When the session expires, as the server wrote it.
        expires_at: String,
    },
    /// Signed out; queue and cache are gone.
    SignedOut {
        /// The request.
        req_id: u64,
    },
    /// A stored queue was thrown away unsent (feature `storage`): it belonged to another user or
    /// site, or it was untagged or unreadable. Each of its calls has already been reported as
    /// `failed` with `auth.session_changed`; the stored value is cleared in the same step.
    QueueDiscarded {
        /// How many calls the queue held.
        count: u64,
        /// Why it was discarded.
        reason: DiscardReason,
    },
    /// The server answered 401 mid-flight: sign in again and the request is retried once.
    ReauthRequired {
        /// The request that is held.
        req_id: u64,
    },
    /// A call succeeded.
    Done {
        /// The request.
        req_id: u64,
        /// The status.
        status: u16,
        /// The body.
        body: String,
        /// True when the server replayed a stored answer for the same `Idempotency-Key`.
        replayed: bool,
    },
    /// A document or list read succeeded.
    Resource {
        /// The request.
        req_id: u64,
        /// The body.
        body: String,
        /// True when the body is the cached copy confirmed by a 304.
        from_cache: bool,
    },
    /// A write lost to a newer version (`412 document.stale_version`): reload and redo.
    Conflict {
        /// The request.
        req_id: u64,
        /// The version to reload, when the server named it.
        current_version: Option<u64>,
    },
    /// A request failed for good.
    Failed {
        /// The request.
        req_id: u64,
        /// The HTTP status, 0 when there was no reply.
        status: u16,
        /// The parsed problem.
        problem: Problem,
    },
    /// Join this room (the transport sends `join`), resuming after `resume_after`.
    RealtimeJoin {
        /// The room.
        room: String,
        /// The last `seq` seen, if any.
        resume_after: Option<u64>,
    },
    /// What a document view shows may be out of date: refetch it.
    Invalidated {
        /// The DocType.
        doctype: String,
        /// The document id.
        id: String,
    },
    /// Events were lost for this room: refetch everything shown from it.
    Resync {
        /// The room.
        room: String,
    },
    /// A notification, once per `event_id`.
    Notification {
        /// The committed event's id.
        event_id: String,
        /// The notice class.
        class: String,
        /// The notice data.
        data: serde_json::Value,
    },
}
