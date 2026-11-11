//! The session seam of `GET /api/v1/auth/sessions` and `DELETE /api/v1/auth/sessions/{id}`
//! (`auth_session_list` and `auth_session_end`, WP-IAM-07.T2): a signed-in user lists and ends
//! their own sessions (ADR-070 section 5.4).
//!
//! `spot-http` does not hold sessions. It defines [`SessionPort`], the two operations in the
//! shapes the router needs, and the host writes the adapter over the session manager. Everything
//! that decides is the port's: who the caller is (from `ctx.credential` on `ctx.site`), which
//! sessions are the caller's, whether a bearer may manage sessions at all (an API key is not a
//! person and may not), and the audit of an ending. The routes pass **no user id**: there is no
//! way to ask for another user's sessions through this trait.
//!
//! An id that is unknown, an id that belongs to another user and an id that is already ended are
//! one answer, `auth.session_not_found`, so an id alone reveals nothing about other users.
//!
//! No session token, token digest or hash is part of [`SessionOut`]: the type has no field for one.

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`SessionPort`].
pub const KEY: PortKey = PortKey::of::<dyn SessionPort>();

/// What a port call answers.
pub type SessionResult<T> = Result<T, ApiError>;

/// One live session of the caller (the contract's `SessionRecord`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOut {
    /// The session's opaque id. Never the bearer token.
    pub id: String,
    /// When the session started, RFC 3339 UTC.
    pub created_at: String,
    /// When the session stops working at the latest, RFC 3339 UTC.
    pub expires_at: String,
    /// When the session was last used, RFC 3339 UTC.
    pub last_used_at: Option<String>,
    /// How the session signed in: `password`, `sso` or `api_key`.
    pub method: String,
    /// Where the session began, as the site records it (a salted digest, never the raw address).
    pub source: String,
    /// The device label, when one was recorded.
    pub device: Option<String>,
    /// Whether this is the session the request is served under.
    pub current: bool,
}

/// The sessions of the signed-in user.
pub trait SessionPort: Send + Sync {
    /// The caller's own live sessions: the current one first, then the others newest first.
    /// A bearer that is not a person (an API key) is refused.
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, SessionResult<Vec<SessionOut>>>;

    /// Ends the caller's session with this id; ending the current one equals logout. An id that
    /// is unknown, another user's or already ended answers `auth.session_not_found`, always the
    /// same answer. A bearer that is not a person is refused.
    fn end<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, SessionResult<()>>;
}
