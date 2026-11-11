//! The egress-guarded HTTP transport, behind the crate's `egress` feature.
//!
//! This is what the real providers (T2, T3) drive. A model endpoint is configured data (bring
//! your own key, a local model, OpenRouter, any compatible endpoint), and every request carries
//! the tenant's API key, so the destination is never trusted:
//!
//! - **Every request is authorised** by the egress [`Guard`] for the tenant and purpose given at
//!   construction, and the connection goes to the pinned addresses the guard vetted and to no
//!   others. The transport has no client of its own and resolves no name, so an endpoint that
//!   points at an internal or metadata address is refused with no connection, and a host that
//!   rebinds between the check and the connect cannot be reached at the new address.
//! - **Every redirect hop is authorised again**, and is followed only inside the original origin
//!   (same scheme, host and port): a redirect elsewhere, including a downgrade to plain HTTP,
//!   is refused, so the key never leaves the endpoint it was configured for.
//! - Per-request timeouts (the request's own timeout bounds the whole exchange) and the
//!   [`COMPLETE_BODY_CAP`] response cap on buffered reads stay in this module, not in callers.
//!
//! # Purpose and tenant
//!
//! The tenant names whose egress policy applies; the purpose names why the call is made. For a
//! model call the purpose is [`Purpose::Llm`] (`"llm"` in policy and in denial records). A tenant
//! with an allow-list entry for `llm` reaches only what the entry lists; a tenant with **no**
//! policy entry is denied outright (the URL path of the guard fails closed), so every tenant
//! that uses a model needs an `llm` allow entry.
//!
//! # No secrets in errors
//!
//! A refusal names the egress code (`egress.denied_address`, ...) and nothing else: no URL, no
//! header, no body, so the API key can appear in no error value and no log line this module
//! produces.
//!
//! # Streaming
//!
//! The egress sender buffers a response before returning it. [`send_stream`] therefore sends
//! through the guard, buffers the body under [`STREAM_BODY_CAP`] and hands it back as chunks:
//! it is guarded and bounded, but not incremental. Incremental delivery needs a streaming
//! method on the egress sender.
//!
//! [`send_stream`]: super::HttpTransport::send_stream

use std::sync::Arc;
use std::time::Duration;

use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use spot_egress::{
    Authorized, Bytes, EgressConnector, EgressError, Guard, HttpRequest as WireRequest, HttpSender,
    Method, Purpose, Request, Response, Sender,
};
use url::Url;

use super::{ByteStream, COMPLETE_BODY_CAP, HttpMethod, HttpRequest, HttpResponse, TransportError};
use crate::provider::BoxFuture;

/// The refusal detail when a redirect leaves the original origin.
const CROSS_ORIGIN_REDIRECT: &str = "a redirect to another origin was refused";

/// How many same-origin redirects one request may take.
const MAX_REDIRECTS: usize = 3;

/// The largest body [`send_stream`](super::HttpTransport::send_stream) buffers: 16 MiB. The
/// whole body is held in memory per request until the sender can stream, so the cap stays well
/// above any model response and well below what concurrent requests could exhaust.
pub const STREAM_BODY_CAP: u64 = 16 * 1024 * 1024;

/// The chunk size a buffered stream body is handed back in.
const STREAM_CHUNK: usize = 16 * 1024;

/// The total time one request may take when the request names no timeout of its own.
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Maps an egress failure onto the transport errors. Only the stable egress code is kept.
fn map_egress(error: &EgressError) -> TransportError {
    match error {
        EgressError::ResolutionFailed => TransportError::Connection("could not connect".to_owned()),
        EgressError::ResponseTooLarge => TransportError::TooLarge {
            cap: COMPLETE_BODY_CAP,
        },
        other => TransportError::Protocol(format!("the egress policy refused: {}", other.code())),
    }
}

/// The egress-guarded HTTP transport: every request and every redirect hop is authorised, the
/// connection goes to the pinned addresses only, redirects stay inside the original origin, and
/// buffered reads are capped at [`COMPLETE_BODY_CAP`].
pub struct EgressTransport {
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    tenant: String,
    purpose: Purpose,
    complete_body_cap: u64,
}

impl EgressTransport {
    /// A transport that authorises with `guard` for `tenant` and `purpose` (use
    /// [`Purpose::Llm`] for model calls) and sends through `sender`.
    pub fn new(
        guard: Arc<Guard>,
        sender: Arc<dyn Sender>,
        tenant: impl Into<String>,
        purpose: Purpose,
    ) -> Self {
        Self {
            guard,
            sender,
            tenant: tenant.into(),
            purpose,
            complete_body_cap: COMPLETE_BODY_CAP,
        }
    }

    /// The production transport for `tenant`: purpose [`Purpose::Llm`], sent by the egress HTTP
    /// sender over a connector that authorises and pins every connection. `total_timeout`
    /// bounds a request whose own timeout is unset, from connect to the last byte.
    pub fn for_tenant(
        guard: Arc<Guard>,
        tenant: impl Into<String>,
        total_timeout: Duration,
    ) -> Self {
        let tenant = tenant.into();
        let connector = EgressConnector::new(Arc::clone(&guard), tenant.clone(), Purpose::Llm);
        let sender = Arc::new(HttpSender::new(connector, total_timeout));
        Self::new(guard, sender, tenant, Purpose::Llm)
    }

    /// The production transport with the default two-minute total timeout.
    pub fn for_tenant_default(guard: Arc<Guard>, tenant: impl Into<String>) -> Self {
        Self::for_tenant(guard, tenant, DEFAULT_TOTAL_TIMEOUT)
    }

    /// Reads at most `cap` bytes into one buffered response.
    #[must_use]
    pub fn with_body_cap(mut self, cap: u64) -> Self {
        self.complete_body_cap = cap;
        self
    }

    /// Builds the egress request: method, URL, headers, body.
    fn prepare(request: &HttpRequest) -> Result<WireRequest, TransportError> {
        let url = Url::parse(&request.url)
            .map_err(|_| TransportError::Protocol("the request URL is not valid".to_owned()))?;
        let method = match request.method {
            HttpMethod::Get => Method::Get,
            HttpMethod::Post => Method::Post,
            HttpMethod::Put => Method::Put,
            HttpMethod::Delete => Method::Delete,
            HttpMethod::Head => {
                return Err(TransportError::Protocol(
                    "HEAD is not supported by the egress sender".to_owned(),
                ));
            }
        };
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                TransportError::Protocol("a request header name is not valid".to_owned())
            })?;
            let mut value = HeaderValue::from_str(value).map_err(|_| {
                TransportError::Protocol("a request header value is not valid".to_owned())
            })?;
            value.set_sensitive(true);
            headers.append(name, value);
        }
        let body = (!request.body.is_empty()).then(|| Bytes::from(request.body.clone()));
        Ok(WireRequest::new(method, url, headers, body))
    }

    /// Authorises and sends `request`, following redirects inside the original origin; every hop
    /// is authorised again. `max_body` bounds the response.
    async fn exchange(
        &self,
        mut request: WireRequest,
        max_body: u64,
    ) -> Result<Response, TransportError> {
        let origin = request.url.origin();
        let mut hops = 0;
        loop {
            let authorized: Authorized = self
                .guard
                .authorize(&Request {
                    url: request.url.clone(),
                    purpose: self.purpose,
                    tenant: self.tenant.clone(),
                    actor: None,
                })
                .await
                .map_err(|error| map_egress(&error))?;
            let response = self
                .sender
                .send_request(&request, &authorized, max_body)
                .await
                .map_err(|error| match error {
                    EgressError::ResponseTooLarge => TransportError::TooLarge { cap: max_body },
                    other => map_egress(&other),
                })?;
            if !response.status.is_redirection() {
                return Ok(response);
            }
            let Some(location) = response
                .headers
                .get(http::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                return Ok(response);
            };
            if hops >= MAX_REDIRECTS {
                return Err(TransportError::Protocol("too many redirects".to_owned()));
            }
            let next = request.url.join(location).map_err(|_| {
                TransportError::Protocol("a redirect target is not valid".to_owned())
            })?;
            if next.origin() != origin {
                return Err(TransportError::Protocol(CROSS_ORIGIN_REDIRECT.to_owned()));
            }
            // 301, 302 and 303 become a body-less GET, as HTTP specifies; 307 and 308 keep the
            // method and body, which stay inside the origin that was already authorised.
            if matches!(
                response.status,
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
            ) {
                request.method = Method::Get;
                request.body = None;
            }
            request.url = next;
            hops += 1;
        }
    }

    /// Runs `exchange` inside the request's own timeout, when it has one.
    async fn timed(
        &self,
        request: &HttpRequest,
        max_body: u64,
    ) -> Result<Response, TransportError> {
        let wire = Self::prepare(request)?;
        match request.timeout {
            Some(limit) => tokio::time::timeout(limit, self.exchange(wire, max_body))
                .await
                .map_err(|_| TransportError::Timeout)?,
            None => self.exchange(wire, max_body).await,
        }
    }
}

/// Copies response headers out, failing a non-UTF-8 value.
fn headers_of(map: &HeaderMap) -> Result<Vec<(String, String)>, TransportError> {
    map.iter()
        .map(|(name, value)| {
            let value = std::str::from_utf8(value.as_bytes())
                .map_err(|_| TransportError::Protocol("a response header is not UTF-8".to_owned()))?
                .to_owned();
            Ok((name.as_str().to_owned(), value))
        })
        .collect()
}

impl super::HttpTransport for EgressTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let response = self.timed(&request, self.complete_body_cap).await?;
            let headers = headers_of(&response.headers)?;
            let body = String::from_utf8(response.body)
                .map_err(|_| TransportError::Protocol("the body is not UTF-8".to_owned()))?;
            Ok(HttpResponse {
                status: response.status.as_u16(),
                headers,
                body,
            })
        })
    }

    fn send_stream<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<ByteStream, TransportError>> {
        Box::pin(async move {
            let response = self.timed(&request, STREAM_BODY_CAP).await?;
            let chunks: Vec<Result<Vec<u8>, TransportError>> = response
                .body
                .chunks(STREAM_CHUNK)
                .map(|chunk| Ok(chunk.to_vec()))
                .collect();
            Ok(Box::pin(futures_util::stream::iter(chunks)) as ByteStream)
        })
    }
}
