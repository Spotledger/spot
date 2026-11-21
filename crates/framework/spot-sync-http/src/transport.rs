//! The two transports: an in-process loopback and a real socket client.
//!
//! ADR-078 section 1: a binding is a codec and a transport. [`Transport`] is that seam for the
//! client. [`LoopbackTransport`] calls the hub's router in process, with no socket, so the T1
//! simulator suite and the integration tests run through the real handlers and the real client;
//! [`SocketTransport`] dials the hub over TLS, presenting the node's client certificate
//! (ADR-062 section 3).
//!
//! The loopback carries the same [`ClientCertificate`] the TLS layer would extract, so identity
//! resolution runs identically on both paths: a test cannot pass over loopback and fail over a
//! socket because it skipped the directory.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tower::ServiceExt;

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::identity::ClientCertificate;
use crate::wire;

/// One request a client sends, already framed.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// The HTTP method.
    pub method: Method,
    /// The path, without a query string.
    pub path: String,
    /// The query string (already encoded), or empty.
    pub query: String,
    /// The body.
    pub body: Bytes,
    /// The content type, when the body has one.
    pub content_type: Option<String>,
}

impl HttpRequest {
    /// A request.
    pub fn new(method: Method, path: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            method,
            path: path.into(),
            query: String::new(),
            body: body.into(),
            content_type: None,
        }
    }

    /// A `GET` with no body.
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path, Bytes::new())
    }

    /// A `POST` with a body.
    pub fn post(path: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self::new(Method::POST, path, body)
    }

    /// Adds a query string (already encoded).
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// Adds a content type.
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// The request target: the path plus the query, when there is one.
    pub fn target(&self) -> String {
        if self.query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query)
        }
    }
}

/// One response a client received.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body bytes.
    pub body: Bytes,
}

impl HttpResponse {
    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as UTF-8 text.
    pub fn text(&self) -> Result<&str> {
        std::str::from_utf8(&self.body)
            .map_err(|_| Error::invalid_request("the response body is not UTF-8"))
    }
}

/// Where a client's requests go.
pub trait Transport: Send + Sync + 'static {
    /// Sends one request and returns the response, or a transport failure.
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse>>;
}

/// An in-process transport that calls the hub's router with no socket.
#[derive(Clone)]
pub struct LoopbackTransport {
    router: Router,
    certificate: ClientCertificate,
}

impl std::fmt::Debug for LoopbackTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackTransport")
            .field("fingerprint", &self.certificate.fingerprint())
            .finish_non_exhaustive()
    }
}

impl LoopbackTransport {
    /// A loopback that presents `certificate` on every request, exactly as TLS would.
    pub fn new(router: Router, certificate: ClientCertificate) -> Self {
        Self {
            router,
            certificate,
        }
    }
}

impl Transport for LoopbackTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse>> {
        let router = self.router.clone();
        let certificate = self.certificate.clone();
        Box::pin(async move {
            let target = request.target();
            let method = request.method;
            let mut builder = Request::builder().method(method).uri(target);
            if let Some(content_type) = &request.content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            let mut request = builder
                .body(Body::from(request.body))
                .map_err(|error| Error::invalid_request(format!("bad request: {error}")))?;
            request.extensions_mut().insert(certificate);
            let response = router.oneshot(request).await.map_err(|error| {
                Error::unavailable(format!("the loopback router failed: {error}"))
            })?;
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), wire::MAX_BODY_BYTES)
                .await
                .map_err(|error| {
                    Error::unavailable(format!("the response body failed: {error}"))
                })?;
            Ok(HttpResponse { status, body })
        })
    }
}

/// A socket transport that dials the hub over TLS with the node's client certificate.
///
/// Each call opens one connection; connection reuse and keepalive (ADR-078 section 2) are a
/// later work package, and the per-request handshake cost is the only thing that changes when it
/// lands.
#[derive(Clone)]
pub struct SocketTransport {
    address: String,
    server_name: String,
    tls: Arc<rustls::ClientConfig>,
    max_body: usize,
}

impl std::fmt::Debug for SocketTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketTransport")
            .field("address", &self.address)
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl SocketTransport {
    /// A transport dialling `address` (`host:port`) with `server_name` as the TLS name.
    pub fn new(
        address: impl Into<String>,
        server_name: impl Into<String>,
        tls: Arc<rustls::ClientConfig>,
    ) -> Self {
        Self {
            address: address.into(),
            server_name: server_name.into(),
            tls,
            max_body: wire::MAX_BODY_BYTES,
        }
    }
}

impl Transport for SocketTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse>> {
        let address = self.address.clone();
        let server_name = self.server_name.clone();
        let tls = Arc::clone(&self.tls);
        let max_body = self.max_body;
        Box::pin(async move {
            let tcp = TcpStream::connect(&address)
                .await
                .map_err(|error| Error::unavailable(format!("connect to {address}: {error}")))?;
            let name = ServerName::try_from(server_name.clone())
                .map_err(|_| Error::unavailable("the server name is not a DNS name"))?;
            let connector = tokio_rustls::TlsConnector::from(tls);
            let stream = connector
                .connect(name, tcp)
                .await
                .map_err(|error| Error::unavailable(format!("TLS handshake failed: {error}")))?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .map_err(|error| {
                        Error::unavailable(format!("HTTP handshake failed: {error}"))
                    })?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            let target = request.target();
            let method = request.method;
            let mut builder = hyper::Request::builder()
                .method(method)
                .uri(target)
                .header(header::HOST, server_name);
            if let Some(content_type) = &request.content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            let outgoing = builder
                .body(Full::new(request.body))
                .map_err(|error| Error::invalid_request(format!("bad request: {error}")))?;
            let response = sender
                .send_request(outgoing)
                .await
                .map_err(|error| Error::unavailable(format!("the request failed: {error}")))?;
            let status = response.status().as_u16();
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|error| Error::unavailable(format!("the response body failed: {error}")))?
                .to_bytes();
            if body.len() > max_body {
                return Err(Error::batch_too_large(format!(
                    "the response is {} bytes, above the {max_body}-byte limit",
                    body.len()
                )));
            }
            Ok(HttpResponse { status, body })
        })
    }
}
