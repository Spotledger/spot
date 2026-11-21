//! The two loopback listeners of the devtools module: the trace viewer (`GET /dev/traces`) and
//! the debug adapter. Both are blocking `std` loops run on a thread of their own and stopped by
//! a flag; neither is part of the contract router.

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_devtools::{DebugPolicy, DebugServer, DevApi};

/// The header that carries the development token. A token never travels in a URL or an
/// argument, and is never logged.
pub const TOKEN_HEADER: &str = "x-dev-token";

/// The largest request head the viewer reads.
const MAX_HEAD: usize = 16 * 1024;

/// A parsed request line and the token header.
#[derive(Debug, PartialEq, Eq)]
pub struct Head {
    /// The method.
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The raw query string, without `?`.
    pub query: Option<String>,
    /// The development token the caller presented (empty when none).
    pub token: String,
}

/// Parses the head of an HTTP/1.1 request. `None` for anything that is not a request line.
#[must_use]
pub fn parse_head(text: &str) -> Option<Head> {
    let mut lines = text.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?;
    parts.next()?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.to_owned(), None),
    };
    let token = lines
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case(TOKEN_HEADER)
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    Some(Head {
        method,
        path,
        query,
        token,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

fn answer(stream: &mut TcpStream, api: &DevApi, policy: &DebugPolicy, production: bool) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while head.len() < MAX_HEAD && !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&head);
    let response = match parse_head(&text) {
        Some(parsed) => api.handle_checked(
            policy,
            production,
            &parsed.token,
            &parsed.method,
            &parsed.path,
            parsed.query.as_deref(),
        ),
        None => spot_devtools::ApiResponse::text(400, "bad request"),
    };
    let head_only = text.starts_with("HEAD ");
    let body = if head_only {
        ""
    } else {
        response.body.as_str()
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len(),
        body
    );
}

/// Serves the trace viewer on `listener` until `stop` is set. Blocking.
pub fn serve_traces(
    listener: &TcpListener,
    api: &DevApi,
    policy: &DebugPolicy,
    production: bool,
    stop: &AtomicBool,
) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                answer(&mut stream, api, policy, production);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

/// Binds the debug adapter's listener after the policy's checks (release build, loopback unless
/// allowed). The adapter's own `bind` is behind the `dev-server` feature, which this crate must
/// never enable; `serve_pair` is not, so this module runs the accept loop itself.
///
/// # Errors
///
/// A sentence for the operator.
pub fn bind_debug(policy: &DebugPolicy, address: SocketAddr) -> Result<TcpListener, String> {
    policy.check_build().map_err(|e| e.to_string())?;
    policy.check_address(address).map_err(|e| e.to_string())?;
    TcpListener::bind(address)
        .map_err(|e| format!("cannot bind the debug adapter on {address}: {e}"))
}

/// Serves debug sessions on `listener` until `stop` is set. Each connection has its own thread;
/// the token guards every launch (the library's rule).
pub fn serve_debug(listener: &TcpListener, server: &Arc<DebugServer>, stop: &AtomicBool) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let server = Arc::clone(server);
                let _ = std::thread::Builder::new()
                    .name("spot-devtools-dap".to_owned())
                    .spawn(move || {
                        let Ok(reader) = stream.try_clone() else {
                            return;
                        };
                        let reader = BufReader::new(reader);
                        let _ = server.serve_pair(reader, stream);
                    });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_head_is_parsed_with_its_token_and_query() {
        let head = parse_head(
            "GET /dev/traces?request_id=r-1 HTTP/1.1\r\nHost: x\r\nX-Dev-Token: dev-token-0123456789\r\n\r\n",
        )
        .unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/dev/traces");
        assert_eq!(head.query.as_deref(), Some("request_id=r-1"));
        assert_eq!(head.token, "dev-token-0123456789");
    }

    #[test]
    fn a_missing_token_is_the_empty_string_and_garbage_is_none() {
        let head = parse_head("GET /dev/traces HTTP/1.1\r\n\r\n").unwrap();
        assert!(head.token.is_empty());
        assert!(parse_head("nonsense").is_none());
        assert!(parse_head("").is_none());
    }
}
