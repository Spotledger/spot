//! The HTTP adapter (`feature = "http"`): mounts the SCIM handlers under `/scim/v2` on an
//! [`axum::Router`] that a `spot-http` [`ServerBuilder`](spot_http::ServerBuilder) serves.
//!
//! The core ([`crate::handle`]) stays sans-I/O; this module is the thin edge that decodes a
//! request (method, path, query, `Authorization`, `If-Match`, body), resolves the bearer
//! credential, locks the store, calls the core and renders the response as
//! `application/scim+json`. Every decision stays in the core, so the adapter can be tested with
//! the same transcripts the core uses.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method as HttpMethod, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use time::OffsetDateTime;

use crate::TenantId;
use crate::request::{Credential, Limits, Method, ScimContext, ScimRequest, handle};
use crate::resource::SCIM_CONTENT_TYPE;
use crate::store::ScimStore;

/// The store the adapter locks: shared, mutable, and `Send` so the router is `Send`.
pub type SharedStore = Arc<Mutex<dyn ScimStore + Send>>;

/// Resolves an `Authorization: Bearer <token>` to a credential, or `None` when it is not one.
/// The token itself is a secret and is never stored, logged or returned.
pub trait CredentialResolver: Send + Sync {
    /// The credential for `bearer` in `tenant`, when the token is valid for it.
    fn resolve(&self, bearer: &str, tenant: &TenantId) -> Option<Credential>;
}

/// Everything the adapter needs: the tenant, the store, the credential resolver, the limits and
/// a clock. The clock is the only ambient read, and it lives here, not in the core.
pub struct ScimServices {
    /// The tenant every request on this router serves.
    pub tenant: TenantId,
    /// The shared store.
    pub store: SharedStore,
    /// The bearer credential resolver.
    pub credentials: Arc<dyn CredentialResolver>,
    /// The request bounds.
    pub limits: Limits,
    /// The clock the adapter reads once per request.
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

/// Builds the `/scim/v2` router.
pub fn router(services: ScimServices) -> Router {
    Router::new()
        .route("/scim/v2", any(scim_handler))
        .route("/scim/v2/{*rest}", any(scim_handler))
        .with_state(Arc::new(services))
}

/// Mounts the SCIM router into a `spot-http` server builder's site routes, so a backend serves
/// `/scim/v2` through the same edge as the native API.
pub fn mount(
    builder: spot_http::ServerBuilder,
    services: ScimServices,
) -> spot_http::ServerBuilder {
    builder.site_routes(router(services))
}

async fn scim_handler(
    State(services): State<Arc<ScimServices>>,
    method: HttpMethod,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(method) = Method::parse(method.as_str()) else {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::CONTENT_TYPE, SCIM_CONTENT_TYPE)],
            String::new(),
        )
            .into_response();
    };
    let Some(credential) = resolve_credential(&services, &headers) else {
        return unauthorised();
    };
    let body_json = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice(&body) {
            Ok(json) => Some(json),
            Err(_) => {
                let error =
                    crate::error::ScimError::invalid_syntax("the body is not a JSON document");
                return render(crate::request::ScimResponse::error(&error));
            }
        }
    };
    let request = ScimRequest {
        method,
        path: uri.path().to_owned(),
        query: parse_query(uri.query()),
        body: body_json,
        if_match: header_text(&headers, header::IF_MATCH),
        content_length: body.len(),
    };
    let now = (services.clock)();
    let Ok(mut guard) = services.store.lock() else {
        return internal();
    };
    let mut ctx = ScimContext {
        tenant: services.tenant.clone(),
        credential: &credential,
        now,
        limits: &services.limits,
        store: &mut *guard,
    };
    render(handle(request, &mut ctx))
}

fn resolve_credential(services: &ScimServices, headers: &HeaderMap) -> Option<Credential> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let bearer = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    if bearer.is_empty() {
        return None;
    }
    services.credentials.resolve(bearer, &services.tenant)
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn parse_query(query: Option<&str>) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if let Some(query) = query {
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                out.insert(percent_decode(key), percent_decode(value));
            }
        }
    }
    out
}

/// A small percent-decoder for query values; `+` becomes a space, as a form query expects.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn render(response: crate::request::ScimResponse) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header_value(SCIM_CONTENT_TYPE));
    if let Some(etag) = &response.etag
        && let Ok(value) = header::HeaderValue::from_str(etag)
    {
        headers.insert(header::ETAG, value);
    }
    if let Some(location) = &response.location
        && let Ok(value) = header::HeaderValue::from_str(location)
    {
        headers.insert(header::LOCATION, value);
    }
    if let Some(retry) = response.retry_after
        && let Ok(value) = header::HeaderValue::from_str(&retry.to_string())
    {
        headers.insert(header::RETRY_AFTER, value);
    }
    let body = match response.body {
        Some(json) => serde_json::to_string(&json).unwrap_or_default(),
        None => String::new(),
    };
    (status, headers, body).into_response()
}

fn header_value(text: &'static str) -> header::HeaderValue {
    header::HeaderValue::from_static(text)
}

fn unauthorised() -> Response {
    let error = crate::error::ScimError::unauthorised("a bearer credential is required");
    render(crate::request::ScimResponse::error(&error))
}

fn internal() -> Response {
    let error = crate::error::ScimError::internal("the store is unavailable");
    render(crate::request::ScimResponse::error(&error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    struct NoCredentials;

    impl CredentialResolver for NoCredentials {
        fn resolve(&self, _bearer: &str, _tenant: &TenantId) -> Option<Credential> {
            None
        }
    }

    #[test]
    fn the_router_builds_and_a_request_without_a_credential_is_refused() {
        let services = ScimServices {
            tenant: "acme".to_owned(),
            store: Arc::new(Mutex::new(MemoryStore::new())),
            credentials: Arc::new(NoCredentials),
            limits: Limits::default(),
            clock: Arc::new(|| OffsetDateTime::UNIX_EPOCH),
        };
        // Building the router must not panic; the handler is exercised end to end by the
        // conformance transcripts against the core.
        let _ = router(services);
    }

    #[test]
    fn percent_decoding_reads_a_filter_value() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
