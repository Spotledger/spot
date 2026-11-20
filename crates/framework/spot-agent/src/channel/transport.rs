//! The wire: one blocking HTTPS exchange per request over **TLS 1.3**. With a CA file the
//! channel is **pinned to the gateway trust anchor**: only the anchor's certificates are
//! trusted and the system roots are not consulted. Without one the system trust store is
//! used ([`super::sysroots`]). There is no `insecure` option. The agent only connects out; it
//! never listens.
//!
//! [`Transport`] is a trait so the client and the pipeline are tested without sockets; the
//! TLS implementation is exercised against a loopback gateway.

use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::{CertificateDer, ServerName};

use crate::error::AgentError;

/// Largest response accepted, bytes (a poll answer is 20 envelopes of at most 16 KiB).
pub const MAX_RESPONSE: usize = 1 << 20;

/// One request.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// `GET` or `POST`.
    pub method: &'static str,
    /// Path and query, starting with `/`.
    pub path: String,
    /// Extra headers (Host, Content-Length and Connection are added).
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
    /// Read/write timeout for this exchange.
    pub timeout: Duration,
}

/// One response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Headers, names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A header value by lower-case name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The exchange seam.
pub trait Transport: Send + Sync {
    /// Sends one request and reads the whole response.
    ///
    /// # Errors
    /// [`AgentError::Transport`] on any connect, TLS (including a certificate that does
    /// not chain to the trust anchor), I/O or framing failure; the text names the step
    /// only.
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AgentError>;
}

/// Parses the base64 bodies of the PEM `CERTIFICATE` blocks in `pem`.
///
/// # Errors
/// [`AgentError::Transport`] when there is none or one is not base64.
pub fn parse_pem_certificates(pem: &str) -> Result<Vec<Vec<u8>>, AgentError> {
    let bad = || AgentError::Transport("the CA file holds no valid PEM certificate".to_owned());
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in pem.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            current = Some(String::new());
        } else if line == "-----END CERTIFICATE-----" {
            let body = current.take().ok_or_else(bad)?;
            out.push(STANDARD.decode(body.as_bytes()).map_err(|_| bad())?);
        } else if let Some(body) = current.as_mut() {
            body.push_str(line);
        }
    }
    if out.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

/// The TLS 1.3 transport.
pub struct TlsTransport {
    host: String,
    port: u16,
    server_name: ServerName<'static>,
    config: Arc<rustls::ClientConfig>,
}

/// Splits `https://host[:port]` into host and port.
///
/// # Errors
/// [`AgentError::Transport`] for anything else.
pub fn split_url(url: &str) -> Result<(String, u16), AgentError> {
    let bad = || AgentError::Transport("the gateway URL is invalid".to_owned());
    let rest = url.strip_prefix("https://").ok_or_else(bad)?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return Err(bad());
    }
    let (host, port_text) = if let Some(v6) = rest.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or_else(bad)?;
        (h, after.strip_prefix(':'))
    } else if let Some((h, p)) = rest.rsplit_once(':') {
        (h, Some(p))
    } else {
        (rest, None)
    };
    let port = match port_text {
        Some(p) => p.parse::<u16>().map_err(|_| bad())?,
        None => 443,
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok((host.to_owned(), port))
}

impl TlsTransport {
    /// Builds the transport for `url`, trusting exactly the certificates in `ca_pem` (the
    /// system roots are not added: the anchor replaces them).
    ///
    /// # Errors
    /// [`AgentError::Transport`] for a bad URL, bad PEM or an unusable anchor.
    pub fn new(url: &str, ca_pem: &str) -> Result<Self, AgentError> {
        let mut roots = rustls::RootCertStore::empty();
        for der in parse_pem_certificates(ca_pem)? {
            roots
                .add(CertificateDer::from(der))
                .map_err(|_| AgentError::Transport("the CA certificate is unusable".to_owned()))?;
        }
        Self::with_store(url, roots)
    }

    /// Builds the transport for `url`, trusting the DER certificates of the system trust
    /// store (`roots`, from [`super::sysroots::system_roots`]). Certificates rustls cannot
    /// parse are skipped; none usable is an error.
    ///
    /// # Errors
    /// [`AgentError::Transport`] for a bad URL or when no certificate is usable.
    pub fn with_system_roots(url: &str, roots: Vec<Vec<u8>>) -> Result<Self, AgentError> {
        let mut store = rustls::RootCertStore::empty();
        let (added, _ignored) =
            store.add_parsable_certificates(roots.into_iter().map(CertificateDer::from));
        if added == 0 {
            return Err(AgentError::Transport(
                "the system trust store holds no usable certificate; pass --ca".to_owned(),
            ));
        }
        Self::with_store(url, store)
    }

    fn with_store(url: &str, roots: rustls::RootCertStore) -> Result<Self, AgentError> {
        let (host, port) = split_url(url)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| AgentError::Transport("TLS 1.3 is unavailable".to_owned()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = ServerName::try_from(host.clone())
            .map_err(|_| AgentError::Transport("the gateway host name is invalid".to_owned()))?;
        Ok(Self {
            host,
            port,
            server_name,
            config: Arc::new(config),
        })
    }
}

fn io_err(step: &str) -> AgentError {
    AgentError::Transport(format!("{step} failed"))
}

impl Transport for TlsTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AgentError> {
        let addrs: Vec<_> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|_| io_err("resolving the gateway"))?
            .collect();
        let mut tcp = None;
        for addr in addrs {
            if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(15)) {
                tcp = Some(stream);
                break;
            }
        }
        let tcp = tcp.ok_or_else(|| io_err("connecting to the gateway"))?;
        tcp.set_read_timeout(Some(request.timeout))
            .map_err(|_| io_err("configuring the socket"))?;
        tcp.set_write_timeout(Some(request.timeout))
            .map_err(|_| io_err("configuring the socket"))?;
        let conn = rustls::ClientConnection::new(self.config.clone(), self.server_name.clone())
            .map_err(|_| io_err("starting TLS"))?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);

        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n",
            request.method, request.path, self.host
        );
        if !request.body.is_empty() || request.method == "POST" {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                request.body.len()
            ));
        }
        for (k, v) in &request.headers {
            if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
                return Err(io_err("building the request"));
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        tls.write_all(head.as_bytes())
            .map_err(|_| io_err("the TLS handshake or write"))?;
        tls.write_all(&request.body)
            .map_err(|_| io_err("writing the request"))?;
        tls.flush().map_err(|_| io_err("writing the request"))?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match tls.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > MAX_RESPONSE + 16_384 {
                        return Err(AgentError::Transport(
                            "the response is too large".to_owned(),
                        ));
                    }
                }
                // A peer that closes without close_notify after a complete response.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
                Err(_) => return Err(io_err("reading the response")),
            }
        }
        parse_response(&raw)
    }
}

/// Parses a complete HTTP/1.1 response (Content-Length, chunked or until close).
///
/// # Errors
/// [`AgentError::Transport`] for a malformed response.
pub fn parse_response(raw: &[u8]) -> Result<HttpResponse, AgentError> {
    let bad = || AgentError::Transport("the response is malformed".to_owned());
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(bad)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad())?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or_else(bad)?;
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let rest = &raw[split + 4..];
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let body = if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        dechunk(rest).ok_or_else(bad)?
    } else if let Some(len) = get("content-length") {
        let len: usize = len.parse().map_err(|_| bad())?;
        if len > MAX_RESPONSE || rest.len() < len {
            return Err(bad());
        }
        rest[..len].to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > MAX_RESPONSE {
        return Err(bad());
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..eol])
                .ok()?
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}
