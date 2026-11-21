//! The connector and the redirect loop: the only paths that make an outbound connection.
//!
//! A connection is made to the addresses [`Guard::authorize`] vetted and to no others. The name
//! is never resolved a second time (the defence against DNS rebinding), and every redirect hop
//! is authorized again.

use crate::sender::{HttpRequest, Method, Response, Sender};
use crate::{Authorized, EgressError, Guard, Purpose, Request};
use http::{HeaderMap, HeaderName, StatusCode, Uri, header};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpStream;
use tower::Service;
use url::Url;

/// Connects to vetted addresses. Also a `tower::Service<Uri>` for hyper-util's legacy client:
/// the service authorizes the URI for its tenant and purpose, then connects to the pinned
/// addresses and returns the stream.
#[derive(Clone)]
pub struct EgressConnector {
    /// The guard that authorizes every connection.
    pub guard: Arc<Guard>,
    tenant: String,
    purpose: Purpose,
    actor: Option<String>,
}

impl EgressConnector {
    /// A connector for `tenant` and `purpose`.
    pub fn new(guard: Arc<Guard>, tenant: impl Into<String>, purpose: Purpose) -> Self {
        Self {
            guard,
            tenant: tenant.into(),
            purpose,
            actor: None,
        }
    }

    /// Records `actor` in the denial records this connector causes.
    #[must_use]
    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    /// The tenant this connector acts for.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    fn tenant_policy(&self) -> Option<&crate::TenantPolicy> {
        self.guard.policy.tenants.get(&self.tenant)
    }

    /// Authorizes `url` for this connector's tenant and purpose.
    pub async fn authorize(&self, url: &Url) -> Result<Authorized, EgressError> {
        self.guard
            .authorize(&Request {
                url: url.clone(),
                purpose: self.purpose,
                tenant: self.tenant.clone(),
                actor: self.actor.clone(),
            })
            .await
    }

    /// Connects to `authorized.addrs` in order and never to anything else, each attempt bounded
    /// by the tenant's connect timeout.
    pub async fn connect_to_pinned(
        &self,
        authorized: &Authorized,
    ) -> Result<TcpStream, EgressError> {
        let connect_timeout =
            Duration::from_secs(self.tenant_policy().map_or(10, |p| p.connect_timeout_secs));
        for addr in &authorized.addrs {
            if let Ok(Ok(stream)) =
                tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await
            {
                return Ok(stream);
            }
        }
        Err(EgressError::ResolutionFailed)
    }

    /// Fetches `url` with `GET`, following redirects through `sender`.
    ///
    /// Every hop, the first included, is authorized again: a redirect to a metadata or private
    /// address, from https to http (unless the policy allows that destination), or to a
    /// destination the policy does not allow fails with `egress.redirect_denied` before any
    /// connection is made. On a hop to another origin (scheme, host or port differs) every
    /// header outside a small safe set is dropped. The hop count is bounded by `max_hops` and the total body bytes
    /// by the tenant's `max_response_bytes`.
    pub async fn follow_redirects(
        &self,
        sender: &dyn Sender,
        initial_url: &Url,
        headers: HeaderMap,
        max_hops: u32,
    ) -> Result<Response, EgressError> {
        self.follow_request_redirects(
            sender,
            HttpRequest::get(initial_url.clone(), headers),
            max_hops,
        )
        .await
    }

    /// Sends `request`, following redirects through `sender`.
    ///
    /// Every hop, the first included, is authorized again: a redirect to a metadata or private
    /// address, from https to http (unless the policy allows that destination), or to a
    /// destination the policy does not allow fails with `egress.redirect_denied` before any
    /// connection is made. On a hop to another origin (scheme, host or port differs) every
    /// header outside a small safe set is dropped. The hop count is bounded by `max_hops` and the total body bytes
    /// by the tenant's `max_response_bytes`.
    ///
    /// A `301`/`302`/`303` becomes a `GET` without a body, as HTTP specifies, but only after the
    /// new target passes the whole policy again. A `307`/`308` would replay the method and body
    /// to a new place and is **not** followed by default: the response is returned as it is,
    /// unless `request.follow_body_redirects` is set, and even then only after the new target
    /// passes the policy. The body therefore never reaches a destination the caller did not ask
    /// for.
    pub async fn follow_request_redirects(
        &self,
        sender: &dyn Sender,
        request: HttpRequest,
        max_hops: u32,
    ) -> Result<Response, EgressError> {
        let max_bytes = self
            .tenant_policy()
            .map_or(100 * 1024 * 1024, |p| p.max_response_bytes);
        let mut method = request.method;
        let mut body = request.body;
        let mut headers = request.headers;
        let mut url = request.url;
        let follow_body = request.follow_body_redirects;
        let extra_safe = request.redirect_safe_headers;
        let mut total: u64 = 0;
        let mut hop: u32 = 0;
        loop {
            let authorized = match self.authorize(&url).await {
                Ok(a) => a,
                // The denial record already carries the detail; the caller learns only that
                // the redirect was not allowed.
                Err(_) if hop > 0 => return Err(EgressError::RedirectDenied),
                Err(e) => return Err(e),
            };
            let current = HttpRequest {
                method,
                url: url.clone(),
                headers: headers.clone(),
                body: body.clone(),
                follow_body_redirects: follow_body,
                redirect_safe_headers: Vec::new(),
            };
            let response = sender
                .send_request(&current, &authorized, max_bytes.saturating_sub(total))
                .await?;
            total = total.saturating_add(response.body.len() as u64);
            if total > max_bytes {
                return Err(EgressError::ResponseTooLarge);
            }
            if !response.status.is_redirection() {
                return Ok(response);
            }
            let Some(location) = response
                .headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
            else {
                return Ok(response);
            };
            match response.status {
                // The standard rule: a payload is not resent under the old method, so these
                // become a body-less GET.
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => {
                    method = Method::Get;
                    body = None;
                }
                // 307/308 keep the method and would replay the body: refused unless the caller
                // opted in. A request with no body has nothing to replay and follows as before.
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
                    if body.is_some() && !follow_body =>
                {
                    return Ok(response);
                }
                _ => {}
            }
            if hop >= max_hops {
                return Err(EgressError::TooManyRedirects);
            }
            let next = url.join(&location).map_err(|_| EgressError::InvalidUrl)?;
            if next.origin() != url.origin() {
                retain_redirect_safe_headers(&mut headers, &extra_safe);
            }
            url = next;
            hop += 1;
        }
    }
}

/// Headers that survive a redirect to another origin: none of them carries a secret.
const REDIRECT_SAFE_HEADERS: [HeaderName; 11] = [
    header::ACCEPT,
    header::ACCEPT_LANGUAGE,
    header::ACCEPT_ENCODING,
    header::USER_AGENT,
    header::CONTENT_TYPE,
    header::CACHE_CONTROL,
    header::RANGE,
    HeaderName::from_static("x-request-id"),
    HeaderName::from_static("traceparent"),
    HeaderName::from_static("tracestate"),
    header::CONTENT_LENGTH,
];

/// True when a header name must never be forwarded to another origin: `authorization`,
/// `cookie`, `proxy-authorization`, or any name containing `token`, `secret`, `key`, `auth`,
/// `password` or `credential` (the match is on the lower-cased name).
pub fn is_sensitive_header(name: &HeaderName) -> bool {
    // `HeaderName::as_str` is always lower case.
    let n = name.as_str();
    n == "cookie"
        || ["token", "secret", "key", "auth", "password", "credential"]
            .iter()
            .any(|w| n.contains(w))
}

/// Keeps only the safe headers on a hop to another origin: the built-in set, `If-*`, and the
/// caller's `extra` names, except that a sensitive name can never be allowed (it is ignored and
/// counted in a debug log).
fn retain_redirect_safe_headers(headers: &mut HeaderMap, extra: &[HeaderName]) {
    let ignored = extra.iter().filter(|n| is_sensitive_header(n)).count();
    if ignored > 0 {
        tracing::debug!(
            ignored,
            "sensitive names in redirect_safe_headers were ignored"
        );
    }
    let drop: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            is_sensitive_header(name)
                || !(REDIRECT_SAFE_HEADERS.contains(name)
                    || name.as_str().starts_with("if-")
                    || extra.contains(name))
        })
        .cloned()
        .collect();
    for name in drop {
        headers.remove(&name);
    }
}

/// A future that connects to the vetted addresses of a URI.
pub struct EgressFuture {
    inner: Pin<Box<dyn Future<Output = Result<TokioIo<TcpStream>, EgressError>> + Send>>,
}

impl Future for EgressFuture {
    type Output = Result<TokioIo<TcpStream>, EgressError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

impl Service<Uri> for EgressConnector {
    type Response = TokioIo<TcpStream>;
    type Error = EgressError;
    type Future = EgressFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let this = self.clone();
        EgressFuture {
            inner: Box::pin(async move {
                let url: Url = uri
                    .to_string()
                    .parse()
                    .map_err(|_| EgressError::InvalidUrl)?;
                let authorized = this.authorize(&url).await?;
                Ok(TokioIo::new(this.connect_to_pinned(&authorized).await?))
            }),
        }
    }
}
