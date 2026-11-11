//! The client side: minimal HTTP/1.1, HTTP/2 and WebSocket clients written over a plain
//! `TcpStream`, so an idle connection costs the client one socket and no task. That keeps the
//! client's share of the process's resident memory small enough to measure the server by it.
//!
//! These speak just enough protocol to be accepted by the real server: they are not general
//! clients and the server's answers are checked, never trusted.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A fixed, valid `Sec-WebSocket-Key`; the server derives its accept value from it and needs no
/// uniqueness.
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// A fixed masking key. Client frames must be masked; the key has no secrecy to protect here.
const MASK: [u8; 4] = [0x12, 0x34, 0x56, 0x78];

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0_u8; 2048];
    let n = stream.read(&mut chunk).await?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    buf.extend_from_slice(&chunk[..n]);
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// HTTP/1.1
// ---------------------------------------------------------------------------

/// Opens a connection and sets `TCP_NODELAY`, so a small request is not held back.
///
/// # Errors
/// The connect failure.
pub async fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Sends one `GET` and reads the whole answer, keeping the connection open. Returns the status.
///
/// # Errors
/// A write or read failure, a close before the answer is complete, or an answer that is not
/// HTTP/1.1.
pub async fn http1_get(stream: &mut TcpStream, addr: SocketAddr, path: &str) -> io::Result<u16> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: */*\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(512);
    let header_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at + 4;
        }
        read_more(stream, &mut buf).await?;
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let status: u16 = head
        .strip_prefix("http/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| bad("the answer is not an HTTP/1.1 response"))?;
    let length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    if let Some(length) = length {
        while buf.len() < header_end + length {
            read_more(stream, &mut buf).await?;
        }
    } else if head.contains("transfer-encoding: chunked") {
        while find(&buf[header_end..], b"0\r\n\r\n").is_none() {
            read_more(stream, &mut buf).await?;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// HTTP/2 (prior knowledge)
// ---------------------------------------------------------------------------

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn h2_frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(9 + payload.len());
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A literal header field without indexing, name taken from the static table at `name_index`
/// (below 16). Values here are short, so no Huffman coding and a one-byte length.
fn hpack_literal(name_index: u8, value: &str) -> Vec<u8> {
    let mut out = vec![name_index];
    out.push(u8::try_from(value.len()).unwrap_or(0));
    out.extend_from_slice(value.as_bytes());
    out
}

/// Opens an HTTP/2 connection (prior knowledge, no TLS), sends one `GET` on stream 1 and reads the
/// answer to its end, acknowledging the server's settings. The connection stays open.
///
/// # Errors
/// A connect, write or read failure, a `GOAWAY`, or a status other than 200.
pub async fn http2_get(addr: SocketAddr, path: &str) -> io::Result<TcpStream> {
    let mut stream = connect(addr).await?;
    let mut headers = vec![0x82, 0x86]; // :method GET, :scheme http
    headers.extend(hpack_literal(0x04, path)); // :path
    headers.extend(hpack_literal(0x01, &addr.to_string())); // :authority
    let mut out = H2_PREFACE.to_vec();
    out.extend(h2_frame(0x4, 0, 0, &[])); // our (empty) SETTINGS
    out.extend(h2_frame(0x1, 0x5, 1, &headers)); // HEADERS: END_STREAM | END_HEADERS
    stream.write_all(&out).await?;
    let mut buf = Vec::with_capacity(512);
    let mut status_ok = false;
    loop {
        while buf.len() >= 9 {
            let len =
                (usize::from(buf[0]) << 16) | (usize::from(buf[1]) << 8) | usize::from(buf[2]);
            if buf.len() < 9 + len {
                break;
            }
            let (kind, flags) = (buf[3], buf[4]);
            let id = u32::from_be_bytes([buf[5] & 0x7f, buf[6], buf[7], buf[8]]);
            let payload = buf[9..9 + len].to_vec();
            buf.drain(..9 + len);
            match kind {
                0x4 if flags & 0x1 == 0 => {
                    stream.write_all(&h2_frame(0x4, 0x1, 0, &[])).await?;
                }
                0x1 if id == 1 => {
                    status_ok = payload.first() == Some(&0x88); // :status 200, indexed
                    if flags & 0x1 != 0 {
                        return if status_ok {
                            Ok(stream)
                        } else {
                            Err(bad("not a 200"))
                        };
                    }
                }
                0x0 if id == 1 && flags & 0x1 != 0 => {
                    return if status_ok {
                        Ok(stream)
                    } else {
                        Err(bad("not a 200"))
                    };
                }
                0x7 => return Err(bad("the server sent GOAWAY")),
                _ => {}
            }
        }
        read_more(&mut stream, &mut buf).await?;
    }
}

// ---------------------------------------------------------------------------
// WebSocket (`spot.v1`)
// ---------------------------------------------------------------------------

/// One masked client frame (FIN set) of at most 65,535 bytes.
fn ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    match u16::try_from(payload.len()) {
        Ok(n) if n < 126 => out.push(0x80 | u8::try_from(n).unwrap_or(0)),
        Ok(n) => {
            out.push(0x80 | 126);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Err(_) => out.push(0x80),
    }
    out.extend_from_slice(&MASK);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i % 4]));
    out
}

/// A server frame cut from the front of `buf`: `(opcode, payload)`, or `None` when incomplete.
fn take_ws_frame(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0f;
    let (len, head) = match buf[1] & 0x7f {
        126 if buf.len() >= 4 => (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4),
        126 => return None,
        127 => return None, // never sent by this server for these messages
        n => (usize::from(n), 2),
    };
    if buf.len() < head + len {
        return None;
    }
    let payload = buf[head..head + len].to_vec();
    buf.drain(..head + len);
    Some((opcode, payload))
}

/// Reads server frames until a text frame whose JSON `type` is `wanted`; pings are answered.
async fn ws_expect(stream: &mut TcpStream, buf: &mut Vec<u8>, wanted: &str) -> io::Result<()> {
    loop {
        while let Some((opcode, payload)) = take_ws_frame(buf) {
            match opcode {
                0x1 => {
                    let value: serde_json::Value =
                        serde_json::from_slice(&payload).map_err(|_| bad("not JSON"))?;
                    if value.get("type").and_then(|t| t.as_str()) == Some(wanted) {
                        return Ok(());
                    }
                }
                0x9 => stream.write_all(&ws_frame(0xa, &payload)).await?,
                0x8 => return Err(bad("the server closed the WebSocket")),
                _ => {}
            }
        }
        read_more(stream, buf).await?;
    }
}

/// Upgrades to `spot.v1`, authenticates with `token`, and joins `room`. Returns the open socket
/// once the server has answered `ready` and `joined`.
///
/// # Errors
/// A connect, write or read failure, a refused upgrade, or a close before `joined`.
pub async fn ws_join(addr: SocketAddr, token: &str, room: &str) -> io::Result<TcpStream> {
    let mut stream = connect(addr).await?;
    let upgrade = format!(
        "GET /api/v1/realtime HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: spot.v1\r\n\r\n"
    );
    stream.write_all(upgrade.as_bytes()).await?;
    let mut buf = Vec::with_capacity(512);
    let header_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at + 4;
        }
        read_more(&mut stream, &mut buf).await?;
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    if !head.starts_with("http/1.1 101") || !head.contains("sec-websocket-protocol: spot.v1") {
        return Err(bad("the WebSocket upgrade was refused"));
    }
    buf.drain(..header_end);
    let auth = serde_json::json!({"type": "auth", "token": token}).to_string();
    stream.write_all(&ws_frame(0x1, auth.as_bytes())).await?;
    ws_expect(&mut stream, &mut buf, "ready").await?;
    let join = serde_json::json!({"type": "join", "ref": "j", "room": room, "resume_after": null})
        .to_string();
    stream.write_all(&ws_frame(0x1, join.as_bytes())).await?;
    ws_expect(&mut stream, &mut buf, "joined").await?;
    Ok(stream)
}

// ---------------------------------------------------------------------------
// Held connections
// ---------------------------------------------------------------------------

/// The idle connections of a run, held open.
#[derive(Default)]
pub struct Held {
    /// HTTP/1.1 keep-alive connections.
    pub http1: Vec<TcpStream>,
    /// HTTP/2 connections.
    pub http2: Vec<TcpStream>,
    /// WebSocket connections.
    pub websocket: Vec<TcpStream>,
}

/// Which protocol a held socket speaks, so the right polite answers are sent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Speaks {
    Http1,
    Http2,
    WebSocket,
}

/// Answers what the server sends on an idle socket the way a real client would: a WebSocket ping
/// gets a pong and a close gets the echo; an HTTP/2 settings frame or ping gets its
/// acknowledgement (the server's graceful shutdown waits for the ping answer).
fn react(speaks: Speaks, stream: &TcpStream, data: &[u8]) {
    match speaks {
        Speaks::Http1 => {}
        Speaks::WebSocket => {
            let mut data = data.to_vec();
            while let Some((opcode, payload)) = take_ws_frame(&mut data) {
                match opcode {
                    0x9 => {
                        let _ = stream.try_write(&ws_frame(0xa, &payload));
                    }
                    0x8 => {
                        let _ = stream.try_write(&ws_frame(0x8, &payload));
                    }
                    _ => {}
                }
            }
        }
        Speaks::Http2 => {
            let mut at = 0;
            while data.len() >= at + 9 {
                let len = (usize::from(data[at]) << 16)
                    | (usize::from(data[at + 1]) << 8)
                    | usize::from(data[at + 2]);
                if data.len() < at + 9 + len {
                    break;
                }
                let (kind, flags) = (data[at + 3], data[at + 4]);
                let payload = &data[at + 9..at + 9 + len];
                if flags & 0x1 == 0 {
                    if kind == 0x6 {
                        let _ = stream.try_write(&h2_frame(0x6, 0x1, 0, payload));
                    } else if kind == 0x4 {
                        let _ = stream.try_write(&h2_frame(0x4, 0x1, 0, &[]));
                    }
                }
                at += 9 + len;
            }
        }
    }
}

/// Reads whatever is waiting on `stream` and reacts to it. Returns whether the server closed it.
fn drain(speaks: Speaks, stream: &TcpStream) -> bool {
    let mut chunk = [0_u8; 512];
    loop {
        match stream.try_read(&mut chunk) {
            Ok(0) => return true,
            Ok(n) => react(speaks, stream, &chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

impl Held {
    /// How many connections are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.http1.len() + self.http2.len() + self.websocket.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn all(&self) -> impl Iterator<Item = (Speaks, &TcpStream)> {
        self.http1
            .iter()
            .map(|s| (Speaks::Http1, s))
            .chain(self.http2.iter().map(|s| (Speaks::Http2, s)))
            .chain(self.websocket.iter().map(|s| (Speaks::WebSocket, s)))
    }

    /// Answers the server's pings (a WebSocket that sends nothing for twice the heartbeat is
    /// closed for idleness) and drains anything else it sent. Cheap: one non-blocking read per
    /// connection, no task per connection.
    pub fn keep_alive(&self) {
        for (speaks, stream) in self.all() {
            let _ = drain(speaks, stream);
        }
    }

    /// Waits until the server has closed every held connection, or `limit` passes. Returns how
    /// many are still open.
    pub async fn wait_closed(&self, limit: Duration) -> usize {
        let streams: Vec<(Speaks, &TcpStream)> = self.all().collect();
        let mut open = vec![true; streams.len()];
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            for (flag, (speaks, stream)) in open.iter_mut().zip(&streams) {
                if *flag && drain(*speaks, stream) {
                    *flag = false;
                }
            }
            let remaining = open.iter().filter(|o| **o).count();
            if remaining == 0 || tokio::time::Instant::now() >= deadline {
                return remaining;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_frame_is_masked_and_round_trips_through_the_mask() {
        let frame = ws_frame(0x1, b"hello");
        assert_eq!(frame[0], 0x81);
        assert_eq!(frame[1], 0x80 | 5);
        assert_eq!(&frame[2..6], &MASK);
        let unmasked: Vec<u8> = frame[6..]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ MASK[i % 4])
            .collect();
        assert_eq!(unmasked, b"hello");
        let long = ws_frame(0x1, &[7; 300]);
        assert_eq!((long[1], long[2], long[3]), (0x80 | 126, 1, 44));
    }

    #[test]
    fn server_frames_are_cut_from_the_front_and_partial_ones_wait() {
        let mut buf = vec![0x81, 0x02, b'o', b'k', 0x89, 0x00, 0x81];
        assert_eq!(take_ws_frame(&mut buf), Some((0x1, b"ok".to_vec())));
        assert_eq!(take_ws_frame(&mut buf), Some((0x9, Vec::new())));
        assert_eq!(take_ws_frame(&mut buf), None);
        assert_eq!(buf, vec![0x81]);
    }

    #[test]
    fn http2_frames_carry_a_24_bit_length_and_a_stream_id() {
        let frame = h2_frame(0x1, 0x5, 1, &[1, 2, 3]);
        assert_eq!(&frame[..9], &[0, 0, 3, 0x1, 0x5, 0, 0, 0, 1]);
        assert_eq!(&frame[9..], &[1, 2, 3]);
        assert_eq!(hpack_literal(4, "/x"), vec![4, 2, b'/', b'x']);
    }
}
