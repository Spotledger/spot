//! A small typed HTTP client for the native API, so a case reads like a user story.
//!
//! It speaks HTTP/1.1 over a loopback socket (the same way the repository's other socket-level
//! tests do), which keeps every byte a case sends and receives visible: a case can assert on a
//! status, a header, a body field or a raw range request without a client library getting in
//! the way. Error envelopes are decoded into [`Reply::error_code`] so a case says
//! `reply.expect_error("permission.denied")` instead of poking at JSON paths.
//!
//! Session handling: [`Client::login`] returns the token and [`Client::with_token`] produces a
//! client that carries it on every request, exactly as a browser session does.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::Semaphore;

use crate::report::Failure;

/// One parsed HTTP response.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The status code.
    pub status: u16,
    /// Response headers, names lowercased.
    pub headers: Vec<(String, String)>,
    /// The body bytes exactly as received.
    pub body: Vec<u8>,
}

impl Reply {
    /// The body parsed as JSON; `null` for an empty body, and a failure text when it is not
    /// JSON at all (a body that should be JSON but is not is a bug worth reporting loudly).
    pub fn json(&self) -> Json {
        if self.body.is_empty() {
            return Json::Null;
        }
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!(
                "response body is not JSON: {:?}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// The body as text (for the plain-text answers: health endpoints, 404 pages).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as text with credential-bearing JSON fields redacted, for a failure message
    /// that may reach the JUnit artifact: a successful sign-in body carries a live session
    /// token, and a failure assertion must never print it.
    pub fn redacted_text(&self) -> String {
        match serde_json::from_slice::<Json>(&self.body) {
            Ok(mut json) => {
                redact(&mut json);
                json.to_string()
            }
            Err(_) => self.text(),
        }
    }

    /// A response header, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The contract's error code, from the envelope (`error.code`).
    pub fn error_code(&self) -> Option<String> {
        self.json()
            .pointer("/error/code")
            .and_then(Json::as_str)
            .map(str::to_owned)
    }

    /// The contract's error details (`error.details`).
    pub fn error_details(&self) -> Json {
        self.json()
            .pointer("/error/details")
            .cloned()
            .unwrap_or(Json::Null)
    }

    /// The request id this response carries: the `x-request-id` header, or the id stamped in
    /// the envelope.
    pub fn request_id(&self) -> Option<String> {
        self.header("x-request-id")
            .map(str::to_owned)
            .or_else(|| {
                self.json()
                    .pointer("/error/request_id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
            })
            .or_else(|| {
                self.json()
                    .pointer("/request_id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
            })
    }

    /// Asserts the status, naming the body when it differs.
    pub fn expect(&self, status: u16) -> Result<&Self, Failure> {
        if self.status == status {
            return Ok(self);
        }
        Err(Failure::new(format!(
            "expected HTTP {status}, got {} with body {}",
            self.status,
            self.text()
        )))
    }

    /// Asserts an error envelope with `code`, and that its status is not a success.
    pub fn expect_error(&self, code: &str) -> Result<&Self, Failure> {
        if self.status < 400 {
            return Err(Failure::new(format!(
                "expected the error `{code}`, got HTTP {} with body {}",
                self.status,
                self.text()
            )));
        }
        match self.error_code().as_deref() {
            Some(actual) if actual == code => Ok(self),
            Some(actual) => Err(Failure::new(format!(
                "expected the error `{code}`, got `{actual}` (HTTP {}) with body {}",
                self.status,
                self.text()
            ))),
            None => Err(Failure::new(format!(
                "expected the error `{code}`, got HTTP {} without an error envelope: {}",
                self.status,
                self.text()
            ))),
        }
    }

    /// The envelope's `data` member.
    pub fn data(&self) -> Json {
        self.json().get("data").cloned().unwrap_or(Json::Null)
    }

    /// A field of the returned document (`data.<field>`).
    pub fn field(&self, name: &str) -> Json {
        self.data().get(name).cloned().unwrap_or(Json::Null)
    }

    /// The returned document's id.
    pub fn id(&self) -> Result<String, Failure> {
        self.field("id")
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Failure::new(format!("no document id in the response: {}", self.text())))
    }

    /// The returned document's version.
    pub fn version(&self) -> Result<i64, Failure> {
        self.field("version").as_i64().ok_or_else(|| {
            Failure::new(format!(
                "no document version in the response: {}",
                self.text()
            ))
        })
    }

    /// The returned document's `_state`.
    pub fn state(&self) -> Result<String, Failure> {
        self.field("_state")
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Failure::new(format!("no `_state` in the response: {}", self.text())))
    }

    /// The `ETag` header, which every write and read of a document stamps.
    pub fn etag(&self) -> Result<String, Failure> {
        self.header("etag")
            .map(str::to_owned)
            .ok_or_else(|| Failure::new(format!("no ETag header: {:?}", self.headers)))
    }

    /// The page's documents (`data` as an array).
    pub fn rows(&self) -> Result<Vec<Json>, Failure> {
        self.data().as_array().cloned().ok_or_else(|| {
            Failure::new(format!(
                "the page's `data` is not an array: {}",
                self.text()
            ))
        })
    }

    /// The page's `next_cursor`, when there is another page.
    pub fn next_cursor(&self) -> Option<String> {
        self.json()
            .get("next_cursor")
            .and_then(Json::as_str)
            .map(str::to_owned)
    }

    /// The page's `total`, when the query asked for it.
    pub fn total(&self) -> Option<u64> {
        self.json().get("total").and_then(Json::as_u64)
    }
}

/// A native-API client bound to one host and one session.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    host: String,
    token: Option<String>,
    timeout: Duration,
    source: Option<IpAddr>,
    /// A gate shared by clones: at most its permits' worth of requests are in flight at once.
    in_flight: Option<Arc<Semaphore>>,
}

impl Client {
    /// A client for `addr` that resolves the site through the `Host` header.
    pub fn new(addr: SocketAddr, host: impl Into<String>) -> Self {
        Self {
            addr,
            host: host.into(),
            token: None,
            timeout: Duration::from_secs(30),
            source: None,
            in_flight: None,
        }
    }

    /// The same client, sharing `gate` with every other client built from it: a request takes
    /// one permit for its whole round trip, so a case with hundreds of sessions can keep the
    /// number of requests the host is serving at once below what the host's connection pool
    /// can serve (the pool is bounded on purpose and answers "unavailable" when it is full).
    /// The wait for a permit is not part of the request's timeout.
    pub fn with_in_flight_gate(&self, gate: Arc<Semaphore>) -> Self {
        let mut client = self.clone();
        client.in_flight = Some(gate);
        client
    }

    /// The same client connecting from a chosen local address (a loopback alias), so a case
    /// can give its sign-in attempts a source of their own: the throttle is per source.
    pub fn with_source(&self, source: IpAddr) -> Self {
        let mut client = self.clone();
        client.source = Some(source);
        client
    }

    /// The same client, carrying a session token (or an API key) on every request.
    pub fn with_token(&self, token: impl Into<String>) -> Self {
        let mut client = self.clone();
        client.token = Some(token.into());
        client
    }

    /// The same client with no credential: the anonymous visitor.
    pub fn anonymous(&self) -> Self {
        let mut client = self.clone();
        client.token = None;
        client
    }

    /// The same client with a different per-request timeout.
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        let mut client = self.clone();
        client.timeout = timeout;
        client
    }

    /// The client's site host name.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The credential the client sends, for the raw-socket cases.
    pub fn bearer(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The client's bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    // ---------------------------------------------------------------- transport

    /// Sends one request and reads the whole response.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<Reply, Failure> {
        let _permit = match &self.in_flight {
            Some(gate) => Some(
                gate.acquire()
                    .await
                    .map_err(|_| Failure::new(format!("{method} {path}: the gate is closed")))?,
            ),
            None => None,
        };
        let mut stream = match self.source {
            None => TcpStream::connect(self.addr).await,
            Some(source) => {
                let socket = if self.addr.is_ipv4() {
                    TcpSocket::new_v4()
                } else {
                    TcpSocket::new_v6()
                }
                .map_err(|e| Failure::new(format!("{method} {path}: no socket: {e}")))?;
                socket.bind(SocketAddr::new(source, 0)).map_err(|e| {
                    Failure::new(format!("{method} {path}: cannot bind {source}: {e}"))
                })?;
                socket.connect(self.addr).await
            }
        }
        .map_err(|e| {
            Failure::new(format!(
                "{method} {path}: cannot connect to {}: {e}",
                self.addr
            ))
        })?;
        let (content_type, bytes) = match body {
            Some((kind, bytes)) => (kind, bytes),
            None => ("application/json", Vec::new()),
        };
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: l3-http\r\n",
            self.host
        );
        if !bytes.is_empty() || matches!(method, "POST" | "PATCH" | "PUT") {
            request.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
                bytes.len()
            ));
        }
        if let Some(token) = &self.token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        let mut payload = request.into_bytes();
        payload.extend_from_slice(&bytes);
        stream
            .write_all(&payload)
            .await
            .map_err(|e| Failure::new(format!("{method} {path}: write failed: {e}")))?;
        stream.flush().await.ok();

        let read = async {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8_192];
            loop {
                if let Some(reply) = parse(&buffer) {
                    return Ok(reply);
                }
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        return Err(Failure::new(format!("{method} {path}: read failed: {e}")));
                    }
                }
            }
            parse(&buffer).ok_or_else(|| {
                Failure::new(format!(
                    "{method} {path}: the connection closed before a full response arrived ({:?})",
                    String::from_utf8_lossy(&buffer)
                ))
            })
        };
        match tokio::time::timeout(self.timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(Failure::new(format!(
                "{method} {path}: no response within {:?}",
                self.timeout
            ))),
        }
    }

    /// Sends a JSON body.
    pub async fn send_json(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &Json,
    ) -> Result<Reply, Failure> {
        self.send(
            method,
            path,
            headers,
            Some(("application/json", body.to_string().into_bytes())),
        )
        .await
    }

    /// `GET`, no body.
    pub async fn get(&self, path: &str) -> Result<Reply, Failure> {
        self.send("GET", path, &[], None).await
    }

    /// `POST /api/v1/auth/login`.
    pub async fn login(&self, user: &str, password: &str) -> Result<Reply, Failure> {
        self.send_json(
            "POST",
            "/api/v1/auth/login",
            &[],
            &json!({ "user": user, "password": password }),
        )
        .await
    }

    /// Signs in and returns a client carrying the session token. Fails with the server's
    /// answer when the sign-in does not produce a token.
    pub async fn sign_in(&self, user: &str, password: &str) -> Result<Client, Failure> {
        let reply = self.login(user, password).await?;
        reply.expect(200)?;
        let data = reply.data();
        let token = data
            .get("token")
            .and_then(Json::as_str)
            .ok_or_else(|| Failure::new(format!("sign-in answered no token: {}", reply.text())))?;
        Ok(self.with_token(token))
    }

    /// `POST /api/v1/auth/logout`.
    pub async fn logout(&self) -> Result<Reply, Failure> {
        self.send("POST", "/api/v1/auth/logout", &[], None).await
    }

    /// `GET /api/v1/auth/session`.
    pub async fn session(&self) -> Result<Reply, Failure> {
        self.get("/api/v1/auth/session").await
    }

    /// `GET /api/v1/auth/permissions`, optionally narrowed to DocTypes.
    pub async fn permissions(&self, doctypes: &[&str]) -> Result<Reply, Failure> {
        let mut path = String::from("/api/v1/auth/permissions");
        for (index, doctype) in doctypes.iter().enumerate() {
            path.push(if index == 0 { '?' } else { '&' });
            path.push_str("doctype=");
            path.push_str(doctype);
        }
        let reply = self.get(&path).await?;
        // The endpoint prices itself in `data.doctypes`; a 200 with no body would be a bug.
        Ok(reply)
    }

    /// `GET /api/v1/meta/{doctype}`.
    pub async fn meta(&self, doctype: &str) -> Result<Reply, Failure> {
        self.get(&format!("/api/v1/meta/{doctype}")).await
    }

    /// `POST /api/v1/resource/{doctype}`.
    pub async fn create(&self, doctype: &str, body: &Json) -> Result<Reply, Failure> {
        self.send_json("POST", &format!("/api/v1/resource/{doctype}"), &[], body)
            .await
    }

    /// `POST /api/v1/resource/{doctype}` with an `Idempotency-Key`.
    pub async fn create_with_key(
        &self,
        doctype: &str,
        key: &str,
        body: &Json,
    ) -> Result<Reply, Failure> {
        self.send_json(
            "POST",
            &format!("/api/v1/resource/{doctype}"),
            &[("Idempotency-Key", key)],
            body,
        )
        .await
    }

    /// `GET /api/v1/resource/{doctype}/{id}`.
    pub async fn document(&self, doctype: &str, id: &str) -> Result<Reply, Failure> {
        self.get(&format!("/api/v1/resource/{doctype}/{id}")).await
    }

    /// `PATCH /api/v1/resource/{doctype}/{id}` with `If-Match: "version"`.
    pub async fn update(
        &self,
        doctype: &str,
        id: &str,
        version: i64,
        body: &Json,
    ) -> Result<Reply, Failure> {
        let etag = format!("\"{version}\"");
        self.send_json(
            "PATCH",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[("If-Match", &etag)],
            body,
        )
        .await
    }

    /// `PATCH` with an explicit `If-Match` value (a stale or malformed one, for the conflict
    /// and precondition cases).
    pub async fn update_with_etag(
        &self,
        doctype: &str,
        id: &str,
        etag: &str,
        body: &Json,
    ) -> Result<Reply, Failure> {
        self.send_json(
            "PATCH",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[("If-Match", etag)],
            body,
        )
        .await
    }

    /// `PATCH` without `If-Match` at all.
    pub async fn update_unconditional(
        &self,
        doctype: &str,
        id: &str,
        body: &Json,
    ) -> Result<Reply, Failure> {
        self.send_json(
            "PATCH",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[],
            body,
        )
        .await
    }

    /// `DELETE /api/v1/resource/{doctype}/{id}` with `If-Match`.
    pub async fn delete(&self, doctype: &str, id: &str, version: i64) -> Result<Reply, Failure> {
        let etag = format!("\"{version}\"");
        self.send(
            "DELETE",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[("If-Match", &etag)],
            None,
        )
        .await
    }

    /// `GET /api/v1/resource/{doctype}?<query>`.
    pub async fn list(&self, doctype: &str, query: &str) -> Result<Reply, Failure> {
        let path = if query.is_empty() {
            format!("/api/v1/resource/{doctype}")
        } else {
            format!("/api/v1/resource/{doctype}?{query}")
        };
        self.get(&path).await
    }

    /// `POST /api/v1/resource/{doctype}/query`.
    pub async fn query(&self, doctype: &str, body: &Json) -> Result<Reply, Failure> {
        self.send_json(
            "POST",
            &format!("/api/v1/resource/{doctype}/query"),
            &[],
            body,
        )
        .await
    }

    /// A lifecycle action: `post`, `cancel`, `correct` or `rename`.
    pub async fn lifecycle(
        &self,
        doctype: &str,
        id: &str,
        action: &str,
        version: i64,
        body: Option<&Json>,
    ) -> Result<Reply, Failure> {
        let etag = format!("\"{version}\"");
        let path = format!("/api/v1/resource/{doctype}/{id}/{action}");
        match body {
            Some(body) => {
                self.send_json("POST", &path, &[("If-Match", &etag)], body)
                    .await
            }
            None => self.send("POST", &path, &[("If-Match", &etag)], None).await,
        }
    }

    /// `POST /api/v1/resource/{doctype}/{id}/reveal`.
    pub async fn reveal(&self, doctype: &str, id: &str, body: &Json) -> Result<Reply, Failure> {
        self.send_json(
            "POST",
            &format!("/api/v1/resource/{doctype}/{id}/reveal"),
            &[],
            body,
        )
        .await
    }

    /// `POST /api/v1/lookup/{doctype}/{field}`.
    pub async fn lookup(&self, doctype: &str, field: &str, body: &Json) -> Result<Reply, Failure> {
        self.send_json(
            "POST",
            &format!("/api/v1/lookup/{doctype}/{field}"),
            &[],
            body,
        )
        .await
    }

    /// `GET /api/health/live` or `/api/health/ready`.
    pub async fn health(&self, which: &str) -> Result<Reply, Failure> {
        self.get(&format!("/api/health/{which}")).await
    }
}

// -------------------------------------------------------------------- parsing

/// Replaces every credential-bearing member of a JSON value with `[redacted]`.
fn redact(value: &mut Json) {
    match value {
        Json::Object(map) => {
            for (key, value) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "token" | "secret" | "password" | "password_hash"
                ) {
                    *value = Json::from("[redacted]");
                } else {
                    redact(value);
                }
            }
        }
        Json::Array(items) => {
            for item in items {
                redact(item);
            }
        }
        _ => {}
    }
}

fn parse(bytes: &[u8]) -> Option<Reply> {
    let separator = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..separator]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let body = &bytes[separator + 4..];
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.contains("chunked"));
    if chunked {
        let body = dechunk(body)?;
        return Some(Reply {
            status,
            headers,
            body,
        });
    }
    let length: Option<usize> = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok());
    match length {
        Some(length) if body.len() < length => None,
        Some(length) => Some(Reply {
            status,
            headers,
            body: body[..length].to_vec(),
        }),
        // No length and no chunking: the body ends when the connection does (`Connection:
        // close` is what this client sends).
        None => Some(Reply {
            status,
            headers,
            body: body.to_vec(),
        }),
    }
}

fn dechunk(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut rest = bytes;
    let mut out = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Filter;

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_content_length_response_is_read_exactly() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\netag: \"3\"\r\n\r\n{\"a\":1}";
        let reply = parse(raw).expect("a complete response");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"{\"a\":1}".to_vec());
        assert_eq!(reply.header("ETag"), Some("\"3\""));
    }

    #[test]
    fn an_incomplete_content_length_response_keeps_waiting() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 20\r\n\r\n{\"a\":1}";
        assert!(parse(raw).is_none());
    }

    #[test]
    fn a_chunked_response_is_dechunked() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let reply = parse(raw).expect("a complete chunked response");
        assert_eq!(reply.body, b"{\"a\":1}".to_vec());
        assert_eq!(reply.json()["a"], 1);
    }

    #[test]
    fn an_incomplete_chunk_is_not_a_response_yet() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n9\r\n{\"a\"";
        assert!(parse(raw).is_none());
    }

    #[test]
    fn an_error_envelope_decodes_its_code_details_and_request_id() {
        let body = r#"{"error":{"code":"permission.denied","message":"No.","details":{"retry_after":7},"request_id":"req-9"}}"#;
        let reply = reply(403, body);
        assert_eq!(reply.error_code().as_deref(), Some("permission.denied"));
        assert_eq!(reply.error_details()["retry_after"], 7);
        assert_eq!(reply.request_id().as_deref(), Some("req-9"));
        assert!(reply.expect_error("permission.denied").is_ok());
        let wrong = reply
            .expect_error("document.linked")
            .expect_err("wrong code");
        assert!(wrong.message.contains("document.linked"), "{wrong}");
        assert!(reply.expect(403).is_ok());
        assert!(reply.expect(200).is_err());
    }

    #[test]
    fn a_page_exposes_its_cursor_total_and_rows() {
        let body = r#"{"data":[{"id":"a","version":1}],"next_cursor":"c2","total":9}"#;
        let reply = reply(200, body);
        assert_eq!(reply.next_cursor().as_deref(), Some("c2"));
        assert_eq!(reply.total(), Some(9));
        assert_eq!(reply.rows().expect("rows").len(), 1);
    }

    #[test]
    fn a_document_exposes_id_version_state_and_etag() {
        let mut reply = reply(
            201,
            r#"{"data":{"id":"0192","version":4,"_state":"draft"}}"#,
        );
        reply.headers.push(("etag".to_owned(), "\"4\"".to_owned()));
        assert_eq!(reply.id().expect("id"), "0192");
        assert_eq!(reply.version().expect("version"), 4);
        assert_eq!(reply.state().expect("state"), "draft");
        assert_eq!(reply.etag().expect("etag"), "\"4\"");
    }

    #[test]
    fn a_missing_document_field_is_a_failure_not_a_panic() {
        let reply = reply(200, r#"{"data":null}"#);
        let failure = reply.id().expect_err("no id");
        assert!(failure.message.contains("no document id"), "{failure}");
    }

    #[test]
    fn the_request_id_falls_back_to_the_header() {
        let mut reply = reply(200, "{}");
        reply
            .headers
            .push(("x-request-id".to_owned(), "req-1".to_owned()));
        assert_eq!(reply.request_id().as_deref(), Some("req-1"));
    }

    #[test]
    fn a_credential_bearing_body_is_redacted_for_a_failure_message() {
        let body = r#"{"data":{"token":"live-session-token","expires_at":"2026-10-22T00:00:00Z","user":{"id":"u1","email":"a@b.test"}}}"#;
        let redacted = reply(200, body).redacted_text();
        assert!(!redacted.contains("live-session-token"), "{redacted}");
        assert!(redacted.contains("\"[redacted]\""), "{redacted}");
        assert!(redacted.contains("\"email\":\"a@b.test\""), "{redacted}");
    }

    #[test]
    fn the_filter_reads_the_quick_flag() {
        // The environment is process-wide, so this test asserts the parser's two shapes
        // directly rather than mutating it for other tests in the same binary.
        assert!(Filter::All.admits(false));
        assert!(Filter::Quick.admits(true));
        assert!(!Filter::Quick.admits(false));
    }
}
