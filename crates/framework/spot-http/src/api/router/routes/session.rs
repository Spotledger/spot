//! `GET /api/v1/auth/sessions` and `DELETE /api/v1/auth/sessions/{id}` (`auth_session_list` and
//! `auth_session_end`, WP-IAM-07.T2): a signed-in user lists and ends their own sessions
//! (ADR-070 section 5.4).
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request (a bearer is present, the path's id has the contract's
//! `SessionId` shape) and refuse a malformed one before the port is asked. They never decide who
//! the caller is, which sessions are theirs or whether a bearer may manage sessions: the port
//! answers for the caller its credential names, and the routes pass it no user id. An unknown id,
//! another user's id and an already ended id are the port's one answer, `404
//! auth.session_not_found`.
//!
//! The answer carries the fields of the contract's `SessionRecord` and nothing else; the port's
//! type has no field for a token or a digest, so none can be echoed. Every response is
//! `private, no-store`.
//!
//! **Not registered in the route registry yet.** Like the report group before WP-PLT-14.T4, this
//! file does not declare `SERVES`, `routes` and `required_ports`: registering the group would add
//! its operations to the served set and the conformance walk (`tests/conformance.rs`, not owned by
//! this task) would then need a case for each of them. The host mounts the group itself
//! (`services/session.rs` of `spot-cli`) through [`registration`]. To register it, rename
//! [`SESSION_SERVES`], [`session_routes`] and [`session_required_ports`] to `SERVES`, `routes` and
//! `required_ports` and add the walk cases.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use serde_json::{Map, Value as Json, json};

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::session::{SessionOut, SessionPort};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail};

/// The operation ids this group serves once registered.
pub const SESSION_SERVES: &[&str] = &["auth_session_list", "auth_session_end"];

/// The longest session id the contract allows.
const ID_MAX: usize = 64;

/// The ports this group needs: the session port, whose adapter decides identity, ownership and
/// the audit. A group without it is not mounted at all.
pub fn session_required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn SessionPort>()];
    &KEYS
}

/// The session routes.
pub fn session_routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/auth/sessions", get(auth_session_list))
        .route("/api/v1/auth/sessions/{id}", delete(auth_session_end))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "session",
        serves: SESSION_SERVES,
        required_ports: session_required_ports,
        routes: session_routes,
        public: None,
    }
}

/// The port and the request context, or the answer to give without them.
struct Call {
    port: std::sync::Arc<dyn SessionPort>,
    ctx: RequestCtx,
}

fn call(state: &RouteState, site: &SiteContext, headers: &HeaderMap) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn SessionPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

type Handled = Result<Response, Failure>;

fn done(result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// Whether `id` has the contract's `SessionId` shape: `^[A-Za-z0-9_-]{1,64}$`.
fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn record_json(session: &SessionOut) -> Json {
    let mut map = Map::new();
    map.insert("id".to_owned(), Json::from(session.id.clone()));
    map.insert(
        "created_at".to_owned(),
        Json::from(session.created_at.clone()),
    );
    map.insert(
        "expires_at".to_owned(),
        Json::from(session.expires_at.clone()),
    );
    if let Some(last) = &session.last_used_at {
        map.insert("last_used_at".to_owned(), Json::from(last.clone()));
    }
    map.insert("method".to_owned(), Json::from(session.method.clone()));
    map.insert("source".to_owned(), Json::from(session.source.clone()));
    if let Some(device) = &session.device {
        map.insert("device".to_owned(), Json::from(device.clone()));
    }
    map.insert("current".to_owned(), Json::Bool(session.current));
    Json::Object(map)
}

async fn auth_session_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let sessions = call
            .port
            .list(&call.ctx)
            .await
            .map_err(|e| fail(e, request_id))?;
        let data: Vec<Json> = sessions.iter().map(record_json).collect();
        Ok((StatusCode::OK, axum::Json(json!({ "data": data }))).into_response())
    }
    .await;
    done(result)
}

async fn auth_session_end(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        // An id that cannot be a session's is a malformed request, not a lookup. Every caller
        // gets the same answer for it, so it tells nothing about any user's sessions.
        if !is_session_id(&id) {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    "A session id is 1 to 64 letters, digits, underscores or hyphens.",
                ),
                request_id,
            ));
        }
        call.port
            .end(&call.ctx, &id)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    done(result)
}

#[cfg(test)]
mod tests {
    use super::is_session_id;

    #[test]
    fn the_id_shape_is_the_contracts() {
        assert!(is_session_id("abc_DEF-123"));
        assert!(is_session_id(&"a".repeat(64)));
        assert!(!is_session_id(""));
        assert!(!is_session_id(&"a".repeat(65)));
        assert!(!is_session_id("a b"));
        assert!(!is_session_id("a/b"));
        assert!(!is_session_id("a.b"));
        assert!(!is_session_id("é"));
    }
}
