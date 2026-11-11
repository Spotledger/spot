//! The reload status endpoint: the last reloads and errors, for the developer and any tooling.
//!
//! The endpoint is deliberately small: `GET /dev/status` returns the bounded history as JSON.
//! [`StatusStore::handle`] is pure — it takes a request line and returns a rendered response —
//! so the route is tested without a socket; [`StatusServer::serve_until`] drives it over TCP on
//! a background thread for the running server, and stops when the shared flag is set.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::error::{DevError, DevResult};
use crate::reload::ReloadRecord;

/// The status document: what the endpoint returns.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusSnapshot {
    /// How many reload batches were recorded.
    pub reloads: u64,
    /// How many of them failed.
    pub failures: u64,
    /// The most recent record, when there is one.
    pub last: Option<ReloadRecord>,
    /// The bounded history, oldest first.
    pub recent: Vec<ReloadRecord>,
}

/// One rendered HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The reason phrase.
    pub reason: &'static str,
    /// The content type.
    pub content_type: &'static str,
    /// The body.
    pub body: String,
}

impl HttpResponse {
    /// A `200 OK` JSON response.
    #[must_use]
    pub fn json(body: String) -> Self {
        Self {
            status: 200,
            reason: "OK",
            content_type: "application/json",
            body,
        }
    }

    /// A `404 Not Found` response.
    #[must_use]
    pub fn not_found() -> Self {
        Self {
            status: 404,
            reason: "Not Found",
            content_type: "text/plain; charset=utf-8",
            body: "not found\n".to_owned(),
        }
    }

    /// The whole response, headers and body, as it goes on the wire.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.status,
            self.reason,
            self.content_type,
            self.body.len(),
            self.body
        )
    }
}

/// A bounded store of the reload history.
#[derive(Debug)]
struct StatusState {
    reloads: u64,
    failures: u64,
    recent: VecDeque<ReloadRecord>,
}

/// The bounded reload history behind the status endpoint.
pub struct StatusStore {
    capacity: usize,
    state: Mutex<StatusState>,
}

impl StatusStore {
    /// A store keeping the last `capacity` records.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(StatusState {
                reloads: 0,
                failures: 0,
                recent: VecDeque::new(),
            }),
        }
    }

    /// Records one reload.
    pub fn record(&self, record: ReloadRecord) {
        let mut state = self.lock();
        state.reloads += 1;
        if !record.ok {
            state.failures += 1;
        }
        state.recent.push_back(record);
        while state.recent.len() > self.capacity {
            state.recent.pop_front();
        }
    }

    /// The current status document.
    #[must_use]
    pub fn snapshot(&self) -> StatusSnapshot {
        let state = self.lock();
        StatusSnapshot {
            reloads: state.reloads,
            failures: state.failures,
            last: state.recent.back().cloned(),
            recent: state.recent.iter().cloned().collect(),
        }
    }

    /// The status document as JSON. Never fails: a serialisation problem of these owned, plain
    /// values is impossible, and the fallback is still valid JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.snapshot())
            .unwrap_or_else(|_| "{\"error\":\"status encode failed\"}".to_owned())
    }

    /// Handles one HTTP request line (`GET /dev/status HTTP/1.1`). Pure: no I/O, no socket.
    #[must_use]
    pub fn handle(&self, request_line: &str) -> HttpResponse {
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if !matches!(method, "GET" | "HEAD") {
            return HttpResponse {
                status: 405,
                reason: "Method Not Allowed",
                content_type: "text/plain; charset=utf-8",
                body: "method not allowed\n".to_owned(),
            };
        }
        let path = target.split('?').next().unwrap_or_default();
        if path == "/dev/status" || path == "/dev/status/" {
            let body = if method == "HEAD" {
                String::new()
            } else {
                self.to_json()
            };
            HttpResponse::json(body)
        } else {
            HttpResponse::not_found()
        }
    }

    fn lock(&self) -> MutexGuard<'_, StatusState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The TCP endpoint serving [`StatusStore`].
pub struct StatusServer {
    store: Arc<StatusStore>,
    addr: SocketAddr,
}

impl StatusServer {
    /// A server on `addr` serving `store`.
    #[must_use]
    pub fn new(store: Arc<StatusStore>, addr: SocketAddr) -> Self {
        Self { store, addr }
    }

    /// Binds the listener.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when the address cannot be bound.
    pub fn bind(&self) -> DevResult<TcpListener> {
        TcpListener::bind(self.addr).map_err(|error| DevError::Site {
            message: format!("cannot bind the status endpoint on {}: {error}", self.addr),
        })
    }

    /// Serves until `shutdown` is set. Blocking; run it on a thread of its own.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when the listener cannot be bound or a connection cannot be answered.
    pub fn serve_until(&self, shutdown: &AtomicBool) -> DevResult<()> {
        let listener = self.bind()?;
        listener
            .set_nonblocking(true)
            .map_err(|error| DevError::Site {
                message: format!("cannot make the status listener non-blocking: {error}"),
            })?;
        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => self.answer(stream)?,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(DevError::Site {
                        message: format!("the status listener failed: {error}"),
                    });
                }
            }
        }
        Ok(())
    }

    fn answer(&self, mut stream: TcpStream) -> DevResult<()> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).map_err(|error| DevError::Site {
            message: format!("cannot read the status request: {error}"),
        })?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let request_line = request.lines().next().unwrap_or_default();
        let response = self.store.handle(request_line);
        stream
            .write_all(response.render().as_bytes())
            .map_err(|error| DevError::Site {
                message: format!("cannot write the status response: {error}"),
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::classify::ChangeKind;
    use crate::reload::StepName;

    fn record(ok: bool) -> ReloadRecord {
        ReloadRecord {
            step: StepName::Metadata,
            kind: ChangeKind::Metadata,
            paths: vec!["meta/widget.toml".into()],
            ok,
            detail: "detail".into(),
            errors: if ok { vec![] } else { vec!["boom".into()] },
            version: Some(1),
            artifacts: vec![],
            started_at_ms: 0,
            duration_ms: 3,
        }
    }

    #[test]
    fn status_route_returns_the_history() {
        let store = StatusStore::new(8);
        store.record(record(true));
        store.record(record(false));
        let response = store.handle("GET /dev/status HTTP/1.1");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"failures\":1"));
        assert!(response.body.contains("\"reloads\":2"));
    }

    #[test]
    fn unknown_route_is_404_and_head_has_no_body() {
        let store = StatusStore::new(8);
        assert_eq!(store.handle("GET /nope HTTP/1.1").status, 404);
        assert_eq!(store.handle("POST /dev/status HTTP/1.1").status, 405);
        assert!(store.handle("HEAD /dev/status HTTP/1.1").body.is_empty());
    }

    #[test]
    fn history_is_bounded() {
        let store = StatusStore::new(1);
        store.record(record(true));
        store.record(record(true));
        assert_eq!(store.snapshot().recent.len(), 1);
        assert_eq!(store.snapshot().reloads, 2);
    }
}
