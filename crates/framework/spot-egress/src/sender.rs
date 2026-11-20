//! Sending one HTTP/1.1 request to the vetted, pinned addresses of an [`Authorized`] result.

use crate::{Authorized, EgressConnector, EgressError};
#[cfg(feature = "tls")]
use crate::{Purpose, TlsOptions, TlsSummary, tls_options::TlsRuntime};
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method as HttpMethod, StatusCode};
use http_body_util::{BodyExt, Either, Empty, Full, Limited};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use std::time::Duration;
use url::Url;

/// The largest request body the sender puts on the wire, in bytes (10 MiB).
///
/// A larger body fails with `egress.request_too_large` before any connection is made, so a
/// call cannot become a bulk exfiltration channel and the sender's memory stays bounded.
pub const MAX_REQUEST_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// The HTTP method of an outbound request.
///
/// A closed set: only what the platform's callers need is representable, so there is no
/// `CONNECT` and no custom verb. Every method is vetted by the same policy as `GET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`: read a resource, with no body by convention.
    Get,
    /// `POST`: submit a payload.
    Post,
    /// `PUT`: replace a resource.
    Put,
    /// `PATCH`: partially update a resource.
    Patch,
    /// `DELETE`: remove a resource.
    Delete,
}

impl Method {
    /// The method's wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    fn wire(self) -> HttpMethod {
        match self {
            Method::Get => HttpMethod::GET,
            Method::Post => HttpMethod::POST,
            Method::Put => HttpMethod::PUT,
            Method::Patch => HttpMethod::PATCH,
            Method::Delete => HttpMethod::DELETE,
        }
    }
}

/// One outbound request: the method, URL, headers and optional body the caller wants sent.
///
/// `authorize` decides *whether* it may go (from the URL plus the tenant's policy); this value
/// carries *what* goes. The sender owns the framing headers: `Host` is written from the URL and
/// `Content-Length` from `body`, and a caller-supplied `Host`, `Content-Length`,
/// `Transfer-Encoding`, `Connection`, `Keep-Alive`, `Proxy-Connection`, `TE`, `Trailer` or
/// `Upgrade` is dropped, never forwarded, so framing cannot be forged.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// The target URL.
    pub url: Url,
    /// Request headers, except the framing and hop-by-hop headers the sender owns.
    pub headers: HeaderMap,
    /// The request body, when there is one.
    pub body: Option<Bytes>,
    /// Whether a `307`/`308` may replay the method and body to a new destination. Off by
    /// default: a body is never sent to a second place unless the caller asks for it, and even
    /// then only after that destination passes the policy again. A `301`/`302`/`303` is not
    /// governed by this flag: it becomes a body-less `GET`, followed without any body, so no
    /// opt-in is needed to drop a payload.
    pub follow_body_redirects: bool,
    /// Extra, non-secret header names kept on a redirect to another origin, on top of the
    /// built-in safe set. Names that look like credentials (see
    /// [`crate::is_sensitive_header`]) are ignored. Empty by default.
    pub redirect_safe_headers: Vec<HeaderName>,
}

impl HttpRequest {
    /// A `GET` request with no body.
    pub fn get(url: Url, headers: HeaderMap) -> Self {
        Self {
            method: Method::Get,
            url,
            headers,
            body: None,
            follow_body_redirects: false,
            redirect_safe_headers: Vec::new(),
        }
    }

    /// A request with `method`, `headers` and an optional `body`.
    pub fn new(method: Method, url: Url, headers: HeaderMap, body: Option<Bytes>) -> Self {
        Self {
            method,
            url,
            headers,
            body,
            follow_body_redirects: false,
            redirect_safe_headers: Vec::new(),
        }
    }
}

/// An HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    /// Status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body, at most the caller's byte bound.
    pub body: Vec<u8>,
}

/// Sends one request to already-authorized addresses. The redirect loop is generic over it, so
/// tests can script responses and production uses [`HttpSender`].
#[async_trait::async_trait]
pub trait Sender: Send + Sync {
    /// Sends `request` to `authorized.addrs` only, writing the caller's own `Host` and
    /// `Content-Length`. Fails with `egress.request_too_large` before connecting when the body
    /// exceeds [`MAX_REQUEST_BODY_BYTES`], and with `egress.response_too_large` when the
    /// response exceeds `max_body` bytes.
    async fn send_request(
        &self,
        request: &HttpRequest,
        authorized: &Authorized,
        max_body: u64,
    ) -> Result<Response, EgressError>;

    /// Sends `GET url` with `headers` to `authorized.addrs` only; fails with
    /// `egress.response_too_large` when the body exceeds `max_body` bytes.
    async fn send(
        &self,
        url: &Url,
        headers: &HeaderMap,
        authorized: &Authorized,
        max_body: u64,
    ) -> Result<Response, EgressError> {
        self.send_request(
            &HttpRequest::get(url.clone(), headers.clone()),
            authorized,
            max_body,
        )
        .await
    }
}

/// The body type of one outbound request: empty, or exactly the caller's bytes.
type RequestBody = Either<Empty<Bytes>, Full<Bytes>>;

/// The production [`Sender`]: connects to the pinned addresses only (never to the name),
/// keeps `Host` and the TLS server name as the original host, and applies the tenant's total
/// timeout.
pub struct HttpSender {
    connector: EgressConnector,
    total_timeout: Duration,
    /// The TLS configurations, built once. `None` only if the TLS library rejected the fixed
    /// default inputs, in which case every HTTPS request fails closed.
    #[cfg(feature = "tls")]
    tls: Option<TlsRuntime>,
    /// Purpose and actor written into the denial record of a refused TLS connection.
    #[cfg(feature = "tls")]
    refusal_context: Option<(Purpose, Option<String>)>,
}

impl HttpSender {
    /// A sender over `connector`, giving each request `total_timeout` from connect to the last
    /// body byte.
    ///
    /// TLS is today's: the public web roots, no client certificate, TLS 1.2 or later. Use
    /// [`HttpSender::with_tls`] for a private CA, mutual TLS, a TLS 1.3 floor or a pinned name.
    pub fn new(connector: EgressConnector, total_timeout: Duration) -> Self {
        Self {
            connector,
            total_timeout,
            #[cfg(feature = "tls")]
            tls: TlsOptions::public_roots()
                .ok()
                .map(TlsRuntime::from_options),
            #[cfg(feature = "tls")]
            refusal_context: None,
        }
    }

    /// A sender over `connector` whose HTTPS connections follow `options`.
    ///
    /// The options were validated, and their rustls configurations built, when they were
    /// created; the sender keeps those configurations and never rebuilds or alters them. The
    /// options (and so the client key's PEM buffer) are consumed here and wiped. The
    /// authorisation path is unchanged: address vetting, DNS pinning and redirect re-validation
    /// all happen before TLS, and a client identity or a server-name override applies to one
    /// origin only (see [`crate::ClientIdentity`]).
    #[cfg(feature = "tls")]
    pub fn with_tls(
        connector: EgressConnector,
        total_timeout: Duration,
        options: TlsOptions,
    ) -> Self {
        Self {
            connector,
            total_timeout,
            tls: Some(TlsRuntime::from_options(options)),
            refusal_context: None,
        }
    }

    /// Records a refused TLS connection (a failed handshake: wrong name, untrusted CA, version
    /// floor, missing client certificate) in the guard's denial sink under `purpose` and
    /// `actor`. The record's rule says `tls_refused`, plus the server-name override when one was
    /// applied to that connection. Without this call a TLS refusal is returned as an error only.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn with_denial_context(mut self, purpose: Purpose, actor: Option<String>) -> Self {
        self.refusal_context = Some((purpose, actor));
        self
    }

    /// A secret-free summary of the TLS options in force (`None` only if the TLS library
    /// rejected the fixed defaults).
    #[cfg(feature = "tls")]
    pub fn tls_summary(&self) -> Option<TlsSummary> {
        self.tls.as_ref().map(TlsRuntime::summary)
    }

    /// Whether the sender writes `name` itself and never copies the caller's value.
    fn is_sender_owned(name: &HeaderName) -> bool {
        const OWNED: [&str; 9] = [
            "host",
            "content-length",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "proxy-connection",
            "te",
            "trailer",
            "upgrade",
        ];
        OWNED.contains(&name.as_str())
    }

    async fn exchange<S>(
        stream: S,
        request: &HttpRequest,
        max_body: u64,
    ) -> Result<Response, EgressError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake::<_, RequestBody>(TokioIo::new(stream))
                .await
                .map_err(|_| EgressError::ResolutionFailed)?;
        let driver = tokio::spawn(async move {
            let _ = conn.await;
        });

        let url = &request.url;
        let host = url.host_str().ok_or(EgressError::InvalidUrl)?;
        let host_header = match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_owned(),
        };
        let path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let body = match &request.body {
            Some(bytes) => Either::Right(Full::new(bytes.clone())),
            None => Either::Left(Empty::new()),
        };
        let mut http_request = http::Request::builder()
            .method(request.method.wire())
            .uri(path)
            .header(header::HOST, host_header)
            .body(body)
            .map_err(|_| EgressError::InvalidUrl)?;
        for (name, value) in &request.headers {
            if !Self::is_sender_owned(name) {
                http_request
                    .headers_mut()
                    .append(name.clone(), value.clone());
            }
        }
        if let Some(bytes) = &request.body {
            // State the size ourselves, from the bytes actually sent. `Transfer-Encoding` and
            // any caller `Content-Length` are already gone, so exactly one length goes out.
            let value = HeaderValue::from_str(&bytes.len().to_string())
                .map_err(|_| EgressError::InvalidUrl)?;
            http_request
                .headers_mut()
                .insert(header::CONTENT_LENGTH, value);
        }

        let result = async {
            let response = sender
                .send_request(http_request)
                .await
                .map_err(|_| EgressError::ResolutionFailed)?;
            let (parts, body) = response.into_parts();
            let limit = usize::try_from(max_body).unwrap_or(usize::MAX);
            let collected = Limited::new(body, limit)
                .collect()
                .await
                .map_err(|_| EgressError::ResponseTooLarge)?;
            Ok(Response {
                status: parts.status,
                headers: parts.headers,
                body: collected.to_bytes().to_vec(),
            })
        }
        .await;
        driver.abort();
        result
    }
}

#[async_trait::async_trait]
impl Sender for HttpSender {
    async fn send_request(
        &self,
        request: &HttpRequest,
        authorized: &Authorized,
        max_body: u64,
    ) -> Result<Response, EgressError> {
        // Bounded before any connection is made, so an over-large body never leaves the process.
        if let Some(body) = &request.body {
            let len = body.len() as u64;
            if len > MAX_REQUEST_BODY_BYTES {
                return Err(EgressError::RequestTooLarge {
                    limit: MAX_REQUEST_BODY_BYTES,
                });
            }
        }
        let work = async {
            let stream = self.connector.connect_to_pinned(authorized).await?;
            match request.url.scheme() {
                "http" => Self::exchange(stream, request, max_body).await,
                #[cfg(feature = "tls")]
                "https" => {
                    let tls = self.tls_connect(stream, request, authorized).await?;
                    Self::exchange(tls, request, max_body).await
                }
                _ => Err(EgressError::InvalidUrl),
            }
        };
        tokio::time::timeout(self.total_timeout, work)
            .await
            .map_err(|_| EgressError::ResolutionFailed)?
    }
}

#[cfg(feature = "tls")]
impl HttpSender {
    /// TLS over an already-connected pinned stream. The certificate is verified against the
    /// original host (or, for the peer origin only, the configured override); the connection was
    /// already vetted and pinned, so nothing here chooses an address.
    async fn tls_connect(
        &self,
        stream: tokio::net::TcpStream,
        request: &HttpRequest,
        authorized: &Authorized,
    ) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, EgressError> {
        let tls = self.tls.as_ref().ok_or(EgressError::ResolutionFailed)?;
        let host = &authorized.original_host;
        let port = request
            .url
            .port_or_known_default()
            .or_else(|| authorized.addrs.first().map(std::net::SocketAddr::port))
            .unwrap_or(443);
        let selection = tls.select(host, port);
        let name = rustls::pki_types::ServerName::try_from(selection.server_name.clone())
            .map_err(|_| EgressError::InvalidUrl)?;
        match tokio_rustls::TlsConnector::from(selection.config)
            .connect(name, stream)
            .await
        {
            Ok(tls_stream) => Ok(tls_stream),
            Err(_) => {
                self.record_refusal(authorized, port, selection.override_used.as_deref());
                Err(EgressError::ResolutionFailed)
            }
        }
    }

    fn record_refusal(&self, authorized: &Authorized, port: u16, override_used: Option<&str>) {
        let Some((purpose, actor)) = &self.refusal_context else {
            return;
        };
        let rule = match override_used {
            Some(name) => format!("tls_refused server_name_override={name}"),
            None => "tls_refused".to_owned(),
        };
        self.connector.guard.record_tls_refusal(
            self.connector.tenant(),
            *purpose,
            actor.clone(),
            &authorized.original_host,
            port,
            authorized.addrs.first().map(std::net::SocketAddr::ip),
            rule,
        );
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn method_wire_names_are_exact() {
        assert_eq!(Method::Get.as_str(), "GET");
        assert_eq!(Method::Post.wire(), HttpMethod::POST);
        assert_eq!(Method::Put.wire(), HttpMethod::PUT);
        assert_eq!(Method::Patch.wire(), HttpMethod::PATCH);
        assert_eq!(Method::Delete.wire(), HttpMethod::DELETE);
    }

    #[cfg(feature = "tls")]
    #[test]
    fn new_keeps_todays_tls_defaults() {
        let policy = crate::EgressPolicy::default();
        let guard = std::sync::Arc::new(crate::Guard::with_defaults(
            policy,
            std::sync::Arc::new(spot_core::SystemClock::new()),
        ));
        let connector = EgressConnector::new(guard, "t", crate::Purpose::Script);
        let sender = HttpSender::new(connector, Duration::from_secs(1));
        assert_eq!(
            sender.tls_summary(),
            Some(TlsSummary {
                trust: "public_roots",
                client_identity: false,
                min_version: crate::TlsVersion::Tls12,
                server_name_override: false,
            })
        );
    }

    #[test]
    fn framing_and_hop_by_hop_headers_are_sender_owned() {
        for name in [
            header::HOST,
            header::CONTENT_LENGTH,
            header::TRANSFER_ENCODING,
            header::CONNECTION,
            HeaderName::from_static("keep-alive"),
            HeaderName::from_static("proxy-connection"),
            header::TE,
            header::TRAILER,
            header::UPGRADE,
        ] {
            assert!(HttpSender::is_sender_owned(&name), "{name}");
        }
        assert!(!HttpSender::is_sender_owned(&header::AUTHORIZATION));
        assert!(!HttpSender::is_sender_owned(&header::CONTENT_TYPE));
    }
}
