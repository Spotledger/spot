//! A small typed HTTP client for the native API, trimmed from `harness/rust/l3-http`'s client
//! (this crate does not depend on `l3-http`: it is a separate harness, see the crate README).
//! Just enough for this harness's own self-test and for [`crate::seed::documents`]: sign-in,
//! health, create.

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
    /// The body parsed as JSON; `null` for an empty body.
    ///
    /// # Panics
    /// When the body is present but is not valid JSON: a body that should be JSON but is not is
    /// a bug worth failing loudly on, exactly as `l3-http`'s client does.
    #[must_use]
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

    /// The body as text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The envelope's `data` member.
    #[must_use]
    pub fn data(&self) -> Json {
        self.json().get("data").cloned().unwrap_or(Json::Null)
    }

    /// Asserts the status, naming the body when it differs.
    pub fn expect(&self, status: u16) -> Result<&Self, String> {
        if self.status == status {
            return Ok(self);
        }
        Err(format!(
            "expected HTTP {status}, got {} with body {}",
            self.status,
            self.text()
        ))
    }

    /// The returned document's id.
    pub fn id(&self) -> Result<String, String> {
        self.data()
            .get("id")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("no document id in the response: {}", self.text()))
    }
}

/// A native-API client bound to one host and one session.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    host: String,
    token: Option<String>,
    timeout: Duration,
}

impl Client {
    /// A client for `addr` that resolves the site through the `Host` header.
    #[must_use]
    pub fn new(addr: SocketAddr, host: impl Into<String>) -> Self {
        Self {
            addr,
            host: host.into(),
            token: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// The same client, carrying a session token on every request.
    #[must_use]
    pub fn with_token(&self, token: impl Into<String>) -> Self {
        let mut client = self.clone();
        client.token = Some(token.into());
        client
    }

    /// The client's bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Sends one request and reads the whole response.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
    ) -> Result<Reply, String> {
        let mut stream = TcpStream::connect(self.addr)
            .await
            .map_err(|e| format!("{method} {path}: cannot connect to {}: {e}", self.addr))?;
        let bytes = body.map(|body| body.to_string().into_bytes());
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: l3-journal-sync\r\n",
            self.host
        );
        if let Some(bytes) = &bytes {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                bytes.len()
            ));
        }
        if let Some(token) = &self.token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        let mut payload = request.into_bytes();
        if let Some(bytes) = bytes {
            payload.extend_from_slice(&bytes);
        }
        stream
            .write_all(&payload)
            .await
            .map_err(|e| format!("{method} {path}: write failed: {e}"))?;
        stream.flush().await.ok();
        let read = async {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                if let Some(reply) = parse(&buffer) {
                    return Ok(reply);
                }
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    Err(e) => return Err(format!("{method} {path}: read failed: {e}")),
                }
            }
            parse(&buffer).ok_or_else(|| {
                format!("{method} {path}: the connection closed before a full response arrived")
            })
        };
        match tokio::time::timeout(self.timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "{method} {path}: no response within {:?}",
                self.timeout
            )),
        }
    }

    /// `GET`, no body.
    pub async fn get(&self, path: &str) -> Result<Reply, String> {
        self.send("GET", path, None).await
    }

    /// `POST /api/v1/auth/login`.
    pub async fn login(&self, user: &str, password: &str) -> Result<Reply, String> {
        self.send(
            "POST",
            "/api/v1/auth/login",
            Some(&json!({ "user": user, "password": password })),
        )
        .await
    }

    /// Signs in and returns a client carrying the session token.
    pub async fn sign_in(&self, user: &str, password: &str) -> Result<Client, String> {
        let reply = self.login(user, password).await?;
        reply.expect(200)?;
        let data = reply.data();
        let token = data
            .get("token")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("sign-in answered no token: {}", reply.text()))?;
        Ok(self.with_token(token))
    }

    /// `GET /api/v1/auth/session`.
    pub async fn session(&self) -> Result<Reply, String> {
        self.get("/api/v1/auth/session").await
    }

    /// `GET /api/health/live` or `/api/health/ready`.
    pub async fn health(&self, which: &str) -> Result<Reply, String> {
        self.get(&format!("/api/health/{which}")).await
    }

    /// `POST /api/v1/resource/{doctype}`.
    pub async fn create(&self, doctype: &str, body: &Json) -> Result<Reply, String> {
        self.send("POST", &format!("/api/v1/resource/{doctype}"), Some(body))
            .await
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
        None => Some(Reply {
            status,
            headers,
            body: body.to_vec(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_content_length_response_is_read_exactly() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n{\"a\":1}";
        let reply = parse(raw).expect("a complete response");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"{\"a\":1}".to_vec());
    }

    #[test]
    fn an_incomplete_response_keeps_waiting() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 20\r\n\r\n{\"a\":1}";
        assert!(parse(raw).is_none());
    }

    #[test]
    fn a_document_response_exposes_its_id() {
        let reply = Reply {
            status: 201,
            headers: Vec::new(),
            body: br#"{"data":{"id":"doc-1","version":1}}"#.to_vec(),
        };
        assert_eq!(reply.id().expect("id"), "doc-1");
        assert!(reply.expect(201).is_ok());
        assert!(reply.expect(200).is_err());
    }
}
