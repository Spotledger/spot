//! A small HTTP/1.1 client over a TCP socket with connection reuse.
//!
//! One [`Conn`] is one simulated browser's connection: it keeps the socket open between
//! requests (`keep-alive`) and opens a new one when the server closed it, exactly as a browser
//! does. Only plain `http` is spoken; a deployment behind TLS is reached through a tunnel to a
//! loopback port.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Where requests go: the socket address and the `Host` header that selects the site.
#[derive(Debug, Clone)]
pub struct Target {
    /// The server's address.
    pub addr: SocketAddr,
    /// The `Host` header value (the site name, or `host:port` of a real deployment).
    pub host: String,
}

/// A parsed response.
#[derive(Debug, Clone)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The body bytes.
    pub body: Vec<u8>,
}

impl Response {
    /// The body as JSON, or `Null` when it is empty or not JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// Why a request produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
    /// No complete response within the timeout.
    Timeout,
    /// The connection failed or the answer was not HTTP.
    Transport,
}

/// One persistent connection.
#[derive(Default)]
pub struct Conn {
    stream: Option<TcpStream>,
}

impl Conn {
    /// A connection that has not been opened yet.
    pub fn new() -> Self {
        Self { stream: None }
    }

    /// Sends one request and reads its whole response, within `timeout`. A reused connection
    /// that turns out to have been closed by the server is replaced once, silently.
    pub async fn request(
        &mut self,
        target: &Target,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Response, HttpError> {
        let work = async {
            let reused = self.stream.is_some();
            match self.exchange(target, method, path, headers, body).await {
                Err(Failed::StaleBeforeAnyByte) if reused => {
                    self.stream = None;
                    self.exchange(target, method, path, headers, body)
                        .await
                        .map_err(|_| HttpError::Transport)
                }
                other => other.map_err(|_| HttpError::Transport),
            }
        };
        match tokio::time::timeout(timeout, work).await {
            Ok(result) => result,
            Err(_) => {
                self.stream = None;
                Err(HttpError::Timeout)
            }
        }
    }

    async fn exchange(
        &mut self,
        target: &Target,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, Failed> {
        if self.stream.is_none() {
            let stream = TcpStream::connect(target.addr)
                .await
                .map_err(|_| Failed::Other)?;
            let _ = stream.set_nodelay(true);
            self.stream = Some(stream);
        }
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: l9-scale\r\nAccept: application/json\r\n",
            target.host
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if let Some(body) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        } else if matches!(method, "POST" | "PUT" | "PATCH") {
            head.push_str("Content-Length: 0\r\n");
        }
        head.push_str("\r\n");
        let mut payload = head.into_bytes();
        if let Some(body) = body {
            payload.extend_from_slice(body);
        }
        let Some(stream) = self.stream.as_mut() else {
            return Err(Failed::Other);
        };
        if stream.write_all(&payload).await.is_err() {
            self.stream = None;
            return Err(Failed::StaleBeforeAnyByte);
        }
        let outcome = read_response(stream).await;
        match outcome {
            Ok((response, reusable)) => {
                if !reusable {
                    self.stream = None;
                }
                Ok(response)
            }
            Err(failed) => {
                self.stream = None;
                Err(failed)
            }
        }
    }
}

enum Failed {
    /// The socket died before the first byte of the answer: the connection was stale.
    StaleBeforeAnyByte,
    Other,
}

async fn read_response(stream: &mut TcpStream) -> Result<(Response, bool), Failed> {
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        match stream.read(&mut chunk).await {
            Ok(0) if buffer.is_empty() => return Err(Failed::StaleBeforeAnyByte),
            Ok(0) | Err(_) => return Err(Failed::Other),
            Ok(n) => buffer.extend_from_slice(chunk.get(..n).ok_or(Failed::Other)?),
        }
        if buffer.len() > 64 * 1024 && find(&buffer, b"\r\n\r\n").is_none() {
            return Err(Failed::Other);
        }
    };
    let head = Head::parse(buffer.get(..head_end).ok_or(Failed::Other)?).ok_or(Failed::Other)?;
    let mut rest: Vec<u8> = buffer.get(head_end + 4..).unwrap_or(&[]).to_vec();
    let no_body = head.status == 204 || head.status == 304 || (100..200).contains(&head.status);
    let (body, mut reusable) = if no_body {
        (Vec::new(), true)
    } else if head.chunked {
        loop {
            if let Some((body, _)) = decode_chunked(&rest) {
                break (body, true);
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(Failed::Other),
                Ok(n) => rest.extend_from_slice(chunk.get(..n).ok_or(Failed::Other)?),
            }
        }
    } else if let Some(length) = head.content_length {
        while rest.len() < length {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(Failed::Other),
                Ok(n) => rest.extend_from_slice(chunk.get(..n).ok_or(Failed::Other)?),
            }
        }
        rest.truncate(length);
        (rest, true)
    } else {
        // No length: the body runs to the end of the connection.
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Err(_) => return Err(Failed::Other),
                Ok(n) => rest.extend_from_slice(chunk.get(..n).ok_or(Failed::Other)?),
            }
        }
        (rest, false)
    };
    if head.close {
        reusable = false;
    }
    Ok((
        Response {
            status: head.status,
            body,
        },
        reusable,
    ))
}

struct Head {
    status: u16,
    content_length: Option<usize>,
    chunked: bool,
    close: bool,
}

impl Head {
    fn parse(bytes: &[u8]) -> Option<Head> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.split("\r\n");
        let status_line = lines.next()?;
        let mut parts = status_line.split(' ');
        if !parts.next()?.starts_with("HTTP/1.") {
            return None;
        }
        let status: u16 = parts.next()?.parse().ok()?;
        let mut head = Head {
            status,
            content_length: None,
            chunked: false,
            close: status_line.starts_with("HTTP/1.0"),
        };
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => head.content_length = value.parse().ok(),
                "transfer-encoding" => {
                    head.chunked = value.to_ascii_lowercase().contains("chunked");
                }
                "connection" => {
                    if value.eq_ignore_ascii_case("close") {
                        head.close = true;
                    } else if value.eq_ignore_ascii_case("keep-alive") {
                        head.close = false;
                    }
                }
                _ => {}
            }
        }
        Some(head)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Decodes a complete `chunked` body: the payload and the number of bytes consumed, or `None`
/// while the terminating chunk (and its trailers) have not all arrived.
pub fn decode_chunked(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut body = Vec::new();
    let mut at = 0usize;
    loop {
        let line_end = at + find(bytes.get(at..)?, b"\r\n")?;
        let size_text = std::str::from_utf8(bytes.get(at..line_end)?).ok()?;
        let size_text = size_text.split(';').next()?.trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        at = line_end + 2;
        if size == 0 {
            // Trailers end with an empty line.
            loop {
                let end = at + find(bytes.get(at..)?, b"\r\n")?;
                let empty = end == at;
                at = end + 2;
                if empty {
                    return Some((body, at));
                }
            }
        }
        let chunk = bytes.get(at..at + size)?;
        body.extend_from_slice(chunk);
        at += size;
        if bytes.get(at..at + 2)? != b"\r\n" {
            return None;
        }
        at += 2;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[test]
    fn chunked_bodies_decode_only_when_complete() {
        let full = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        let (body, used) = decode_chunked(full).unwrap();
        assert_eq!(body, b"hello world");
        assert_eq!(used, full.len());
        assert!(decode_chunked(&full[..full.len() - 3]).is_none());
        assert!(decode_chunked(b"zz\r\n").is_none());
        let trailers = b"1\r\na\r\n0\r\nX-T: 1\r\n\r\n";
        assert_eq!(decode_chunked(trailers).unwrap().0, b"a");
    }

    #[test]
    fn heads_are_parsed() {
        let head =
            Head::parse(b"HTTP/1.1 201 Created\r\nContent-Length: 12\r\nConnection: close\r\nX: y")
                .unwrap();
        assert_eq!(
            (head.status, head.content_length, head.close),
            (201, Some(12), true)
        );
        assert!(Head::parse(b"SMTP/1.1 200 OK").is_none());
        let chunked = Head::parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked").unwrap();
        assert!(chunked.chunked && !chunked.close);
    }

    /// A server that answers every request on a connection with `reply` and counts the
    /// connections it accepted.
    async fn serve(
        reply: &'static [u8],
        close_after: Option<usize>,
    ) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut served = 0usize;
                    let mut buf = [0u8; 4096];
                    loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        if socket.write_all(reply).await.is_err() {
                            return;
                        }
                        served += 1;
                        if close_after == Some(served) {
                            return;
                        }
                    }
                });
            }
        });
        (addr, accepted)
    }

    fn target(addr: SocketAddr) -> Target {
        Target {
            addr,
            host: "site".into(),
        }
    }

    #[tokio::test]
    async fn requests_reuse_one_connection() {
        let (addr, accepted) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"data\":[1]}",
            None,
        )
        .await;
        let mut conn = Conn::new();
        for _ in 0..3 {
            let r = conn
                .request(
                    &target(addr),
                    "GET",
                    "/x",
                    &[],
                    None,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
            assert_eq!(r.status, 200);
            assert_eq!(r.json()["data"][0], 1);
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_connection_the_server_closed_is_replaced_once() {
        let (addr, accepted) =
            serve(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok", Some(1)).await;
        let mut conn = Conn::new();
        for _ in 0..3 {
            let r = conn
                .request(
                    &target(addr),
                    "POST",
                    "/x",
                    &[("X-Request-Id", "r")],
                    Some(b"{}"),
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
            assert_eq!(r.body, b"ok");
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn chunked_and_close_delimited_answers_are_read() {
        let (addr, _) = serve(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
            None,
        )
        .await;
        let mut conn = Conn::new();
        let r = conn
            .request(&target(addr), "GET", "/", &[], None, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(r.body, b"abc");
    }

    #[tokio::test]
    async fn a_silent_server_is_a_timeout_and_a_closed_port_a_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut conn = Conn::new();
        let err = conn
            .request(
                &target(addr),
                "GET",
                "/",
                &[],
                None,
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert_eq!(err, HttpError::Timeout);
        drop(listener);
        let err = conn
            .request(&target(addr), "GET", "/", &[], None, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err, HttpError::Transport);
    }
}
