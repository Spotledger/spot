//! The password-change gate: until a user chooses a password, a credential that came from a
//! one-time secret can do almost nothing.
//!
//! A principal is *restricted* when either
//!
//! - its session was started by signing in with a one-time first sign-in secret (the session id
//!   is remembered here, across reloads, until the session ends), or
//! - its user still holds a one-time verifier (`first_sign_in`) in the site file. This covers
//!   API keys of that user, a session another node started, and a session that outlives an
//!   operator's `spot set-password` (the first rule keeps that one restricted).
//!
//! A restricted principal may do exactly three things: change the password, sign out, and read
//! its own session (the display name). Everything else is refused with
//! `cli.password_change_required` (HTTP 403) by [`refusal`], called from one place: the
//! dispatcher, before the request reaches the router. The check is an allow-list on method and
//! exact path, so a route added later is refused for a restricted principal until it is
//! listed here.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// The error code of a refused request.
pub const REFUSAL_CODE: &str = "cli.password_change_required";

/// The path that changes the caller's password.
pub const PASSWORD_PATH: &str = "/api/v1/auth/password";

/// Whether a restricted principal may call `method` on `path`.
///
/// Exact match only: no prefix, no trailing slash, no other method (`HEAD` is not `GET`).
pub fn allowed_while_restricted(method: &Method, path: &str) -> bool {
    (method == Method::POST && (path == PASSWORD_PATH || path == "/api/v1/auth/logout"))
        || (method == Method::GET && path == "/api/v1/auth/session")
}

/// The refusal for a restricted principal: 403 with the stable code, never 200.
pub fn refusal(request_id: &str) -> Response {
    let body = json!({
        "error": {
            "code": REFUSAL_CODE,
            "message": "Choose a password before doing anything else.",
        },
        "request_id": request_id,
    });
    (StatusCode::FORBIDDEN, axum::Json(body)).into_response()
}

/// The ids of the sessions a one-time secret started, with when each stops mattering.
///
/// Kept by the host across a reload, like the session store itself. An entry is dropped when its
/// session ends (password change, sign out) or when the session would have expired anyway.
#[derive(Default)]
pub struct RestrictedSessions {
    ids: Mutex<BTreeMap<String, i64>>,
}

impl RestrictedSessions {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, i64>> {
        self.ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remembers a restricted session until `expires_at` (Unix seconds), forgetting the ones
    /// that would have expired by `now`.
    pub fn insert(&self, session: &str, expires_at: i64, now: i64) {
        let mut ids = self.lock();
        ids.retain(|_, until| *until > now);
        ids.insert(session.to_owned(), expires_at);
    }

    /// Whether the session was started by a one-time secret.
    pub fn contains(&self, session: &str) -> bool {
        self.lock().contains_key(session)
    }

    /// Forgets an ended session.
    pub fn remove(&self, session: &str) {
        self.lock().remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_operations_are_allowed_and_only_exactly() {
        let allowed = [
            (Method::POST, "/api/v1/auth/password"),
            (Method::POST, "/api/v1/auth/logout"),
            (Method::GET, "/api/v1/auth/session"),
        ];
        for (method, path) in &allowed {
            assert!(allowed_while_restricted(method, path), "{method} {path}");
        }
        let refused = [
            (Method::GET, "/api/v1/auth/password"),
            (Method::PUT, "/api/v1/auth/password"),
            (Method::GET, "/api/v1/auth/logout"),
            (Method::POST, "/api/v1/auth/session"),
            (Method::HEAD, "/api/v1/auth/session"),
            (Method::GET, "/api/v1/auth/session/"),
            (Method::GET, "/api/v1/auth/session/x"),
            (Method::POST, "/api/v1/auth/password/"),
            (Method::POST, "/API/v1/auth/password"),
            (Method::GET, "/api/v1/auth/permissions"),
            (Method::GET, "/api/v1/resource/user"),
            (Method::POST, "/api/v1/resource/user"),
            (Method::POST, "/api/v1/auth/login"),
            (Method::GET, "/metrics"),
            (Method::GET, "/"),
            (Method::GET, ""),
            (Method::GET, "/api/v1/auth/session?x=1"),
            (Method::OPTIONS, "/api/v1/auth/session"),
            (Method::DELETE, "/api/v1/auth/logout"),
            (Method::PATCH, "/api/v1/auth/password"),
            (Method::CONNECT, "/api/v1/auth/session"),
            (Method::GET, "//api/v1/auth/session"),
            (Method::GET, "/api/v1/auth/%73ession"),
            (Method::GET, "/api/v1/auth/session%2f"),
            (Method::GET, "/api/v1/auth/./session"),
            (Method::GET, "/api/v1/auth/x/../session"),
            (Method::GET, " /api/v1/auth/session"),
            (Method::GET, "/api/v1/auth/session\u{0}"),
        ];
        for (method, path) in &refused {
            assert!(!allowed_while_restricted(method, path), "{method} {path}");
        }
    }

    #[test]
    fn the_refusal_is_403_with_the_stable_code() {
        let response = refusal("req-1");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn restricted_sessions_are_pruned_when_they_would_have_expired() {
        let set = RestrictedSessions::new();
        set.insert("a", 100, 0);
        set.insert("b", 200, 150);
        assert!(!set.contains("a"), "a expired before b was added");
        assert!(set.contains("b"));
        set.remove("b");
        assert!(!set.contains("b"));
    }
}
