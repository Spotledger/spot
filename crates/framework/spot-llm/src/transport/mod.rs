//! The HTTP seam providers use instead of a concrete client.
//!
//! A provider adapter (T2, T3) builds an [`HttpRequest`] — method, URL, headers, body, a
//! per-request timeout — and hands it to a [`HttpTransport`]: either the
//! [`mock::MockTransport`] replaying recorded exchanges from files, or the
//! [`egress::EgressTransport`] behind the crate's `egress` feature. Keeping the client
//! behind this seam keeps unit tests network-free (ADR-45's spirit: the boundary owns its I/O).
//!
//! Two transport rules apply to every implementation:
//! - a buffered `send` never reads more than [`COMPLETE_BODY_CAP`] (8 MiB) of a response body,
//!   while `send_stream` streams without that cap;
//! - a redirect to another origin is refused: a BYOK endpoint must not be followed anywhere else
//!   (WP-AI-03: per-tenant allowed-endpoint setting enforced later, at the router).

#[cfg(feature = "egress")]
pub mod egress;
pub mod mock;

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::provider::BoxFuture;

/// The largest body `send` reads: 8 MiB. `send_stream` carries no such cap.
pub const COMPLETE_BODY_CAP: u64 = 8 * 1024 * 1024;

/// Header names whose values `Debug` redacts wherever headers are printed.
const SENSITIVE_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "cookie",
];

/// An HTTP method a provider adapter can send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `HEAD`
    Head,
}

impl HttpMethod {
    /// The method's canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
        }
    }
}

/// An outbound HTTP request a provider adapter builds. `Debug` redacts sensitive header values
/// and never prints the body; `serde` keeps full values, because recorded exchange files need
/// them to replay faithfully.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// The method.
    pub method: HttpMethod,
    /// The absolute URL.
    pub url: String,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The UTF-8 body (JSON for model APIs, event lines for SSE).
    pub body: String,
    /// The per-request timeout; `None` leaves the transport's own default in charge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

impl HttpRequest {
    /// A POST with a JSON body — the shape every model API takes.
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.into(),
            timeout: None,
        }
    }

    /// Adds a header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sets the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method.as_str())
            .field("url", &self.url)
            .field("headers", &HeaderList(&self.headers))
            .field("body_len", &self.body.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A received HTTP response: status, headers, and the buffered UTF-8 body.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The response headers, in wire order.
    pub headers: Vec<(String, String)>,
    /// The body, buffered under [`COMPLETE_BODY_CAP`].
    pub body: String,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &HeaderList(&self.headers))
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Prints a header list with sensitive values redacted — the only way headers are logged.
struct HeaderList<'a>(&'a [(String, String)]);

impl fmt::Debug for HeaderList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for (name, value) in self.0 {
            let sensitive = SENSITIVE_HEADERS
                .iter()
                .any(|s| s.eq_ignore_ascii_case(name));
            list.entry(&(name.clone(), if sensitive { "***" } else { value.as_str() }));
        }
        list.finish()
    }
}

/// Why a transport call failed. Codes are `transport.*`; `Timeout` and `Connection` are
/// retryable, the rest are not. Details name the failure, never the request body or a header.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TransportError {
    /// The exchange did not finish inside its timeout (`transport.timeout`).
    #[error("transport.timeout: the request did not finish in time")]
    Timeout,
    /// Connection-level failure — DNS, TCP or TLS (`transport.connection`).
    #[error("transport.connection: {0}")]
    Connection(String),
    /// A buffered response body exceeded its cap (`transport.too_large`).
    #[error("transport.too_large: the response body exceeds the {cap} byte cap")]
    TooLarge {
        /// The cap that was exceeded, in bytes.
        cap: u64,
    },
    /// The exchange broke its protocol (`transport.protocol`) — a refused redirect, a bad
    /// header, a body that is not what the transport was promised.
    #[error("transport.protocol: {0}")]
    Protocol(String),
}

impl TransportError {
    /// The dotted machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            TransportError::Timeout => "transport.timeout",
            TransportError::Connection(_) => "transport.connection",
            TransportError::TooLarge { .. } => "transport.too_large",
            TransportError::Protocol(_) => "transport.protocol",
        }
    }

    /// Whether retrying the same request later can succeed.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            TransportError::Timeout | TransportError::Connection(_)
        )
    }
}

/// An open response body as byte chunks — the shape SSE arrives in.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send + 'static>>;

/// The client seam providers use instead of a concrete HTTP client; object-safe over boxed
/// futures, like [`Provider`](crate::Provider).
pub trait HttpTransport: Send + Sync {
    /// Sends `request` and buffers the response body under [`COMPLETE_BODY_CAP`].
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>>;

    /// Sends `request` and returns the response body as an unbounded stream of byte chunks.
    fn send_stream<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<ByteStream, TransportError>>;
}

/// Forwards [`HttpTransport`] through a smart pointer, so `Arc<T>` and `Box<dyn HttpTransport>`
/// can back a provider.
macro_rules! forward_transport {
    ($ptr:ident) => {
        impl<T: HttpTransport + ?Sized> HttpTransport for $ptr<T> {
            fn send<'a>(
                &'a self,
                request: HttpRequest,
            ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
                (**self).send(request)
            }

            fn send_stream<'a>(
                &'a self,
                request: HttpRequest,
            ) -> BoxFuture<'a, Result<ByteStream, TransportError>> {
                (**self).send_stream(request)
            }
        }
    };
}

forward_transport!(Arc);
forward_transport!(Box);
