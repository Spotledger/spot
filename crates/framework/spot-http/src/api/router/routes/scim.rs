//! `/scim/v2` (WP-IAM-06.T2): the SCIM 2.0 server of ADR-070 sections 7.2 and 7.3 as one route
//! group over [`ScimPort`].
//!
//! # What the route decides, and what it does not
//!
//! The route is a thin edge: it takes the method, the path, the decoded query, `If-Match` and the
//! body bytes (bounded by [`ScimPort::max_body_bytes`]) and hands them to the port with the
//! request context. It never decides who the caller is: the port authenticates the bearer as a
//! **SCIM key** (a key whose scope is exactly `scim`), so a native session or a native key is
//! answered `401` and a SCIM key is refused by the native API (it has no `scim` right there).
//! The group is mounted at `/scim/v2`, **not** under `/api/v1`, and only when the host provides
//! the port (the host does so only when `services.scim.enabled = true`).
//!
//! # Wire format
//!
//! Every answer is `application/scim+json`, errors included, in the RFC 7644 form
//! ([`ScimReply::error`]); no native error envelope is ever used here. A body larger than the
//! port's bound is answered `413` before the port is called, and the route logs nothing: not a
//! header, not a token, not a byte of the body.
//!
//! This group serves no contract operation (the native contract has no SCIM operations); its one
//! `SERVES` id is the group's own name, so the registry can report it.

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::scim::{SCIM_CONTENT_TYPE, ScimCall, ScimPort, ScimReply};
use crate::site::SiteContext;

use super::super::build_ctx;

/// The group's identifier (it serves no native contract operation).
pub const SERVES: &[&str] = &["scim_v2"];

/// The ports this group needs: the SCIM port. A group without it is not mounted.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn ScimPort>()];
    &KEYS
}

/// The SCIM routes: `/scim/v2` and everything below it, any method (the port answers `405` in
/// RFC form for a method a resource does not take).
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/scim/v2", any(serve))
        .route("/scim/v2/{*rest}", any(serve))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "scim",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

fn render(reply: ScimReply) -> Response {
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, Body::from(reply.body)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(SCIM_CONTENT_TYPE),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    let mut put = |name: header::HeaderName, value: Option<String>| {
        if let Some(value) = value
            && let Ok(value) = HeaderValue::from_str(&value)
        {
            headers.insert(name, value);
        }
    };
    put(header::ETAG, reply.etag);
    put(header::LOCATION, reply.location);
    put(
        header::RETRY_AFTER,
        reply.retry_after.map(|s| s.to_string()),
    );
    response
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn serve(
    State(state): State<RouteState>,
    site: SiteContext,
    method: Method,
    uri: Uri,
    query: Result<Query<BTreeMap<String, String>>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let Some(port) = state.port::<dyn ScimPort>() else {
        return render(ScimReply::error(500, None, "SCIM is not available."));
    };
    let Ok(Query(query)) = query else {
        return render(ScimReply::error(
            400,
            Some("invalidSyntax"),
            "The query string is not valid.",
        ));
    };
    let max = port.max_body_bytes();
    let body = match axum::body::to_bytes(request.into_body(), max.saturating_add(1)).await {
        Ok(bytes) if bytes.len() <= max => bytes.to_vec(),
        _ => {
            return render(ScimReply::error(
                413,
                Some("tooLarge"),
                "The request body is larger than this endpoint accepts.",
            ));
        }
    };
    let ctx = build_ctx(&site, &headers, None);
    let call = ScimCall {
        method: method.as_str().to_ascii_uppercase(),
        path: uri.path().to_owned(),
        query,
        if_match: header_text(&headers, header::IF_MATCH),
        body,
    };
    render(port.serve(&ctx, call).await)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_group_serves_one_id_and_needs_one_port() {
        assert_eq!(SERVES, &["scim_v2"]);
        assert_eq!(required_ports().len(), 1);
        assert_eq!(registration().name, "scim");
    }

    #[test]
    fn a_reply_renders_as_scim_json_and_never_cached() {
        let response = render(ScimReply {
            status: 201,
            body: "{}".to_owned(),
            etag: Some("W/\"1\"".to_owned()),
            location: Some("/scim/v2/Users/x".to_owned()),
            retry_after: None,
        });
        assert_eq!(response.status(), StatusCode::CREATED);
        let h = response.headers();
        assert_eq!(h[header::CONTENT_TYPE], SCIM_CONTENT_TYPE);
        assert_eq!(h[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(h[header::ETAG], "W/\"1\"");
        assert_eq!(h[header::LOCATION], "/scim/v2/Users/x");
        assert!(h.get(header::RETRY_AFTER).is_none());
    }

    #[test]
    fn an_error_reply_carries_retry_after_when_rate_limited() {
        let mut reply = ScimReply::error(429, Some("tooMany"), "slow down");
        reply.retry_after = Some(12);
        let response = render(reply);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "12");
    }
}
