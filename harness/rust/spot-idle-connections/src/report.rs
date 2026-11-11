//! The report of a run: measurements, the verdict of each budget, and the JSON printed by the
//! ignored 10k test.

use serde::Serialize;

use crate::stats::Percentiles;

/// One budget and whether the run stayed under it.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// A stable name: `connections`, `accept_errors`, `memory`, `latency`, `busy_errors`,
    /// `shutdown`, `tasks`.
    pub name: &'static str,
    /// Whether the run met it.
    pub ok: bool,
    /// The numbers behind the verdict, in words.
    pub detail: String,
}

/// What was asked of the run.
#[derive(Debug, Clone, Serialize)]
pub struct Setup {
    /// Idle connections requested.
    pub connections: usize,
    /// HTTP/1.1 share of them.
    pub http1: usize,
    /// HTTP/2 share.
    pub http2: usize,
    /// WebSocket share.
    pub websocket: usize,
    /// Hold and baseline length, seconds.
    pub hold_seconds: u64,
    /// Busy clients.
    pub busy_clients: usize,
    /// Busy requests per second over all clients.
    pub busy_rps: u32,
    /// Shutdown grace, seconds.
    pub shutdown_grace_seconds: u64,
}

/// Process memory around the idle connections. Resident size of this process, which holds the
/// server and the clients; the clients keep one socket per connection and no task.
#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    /// How the number was read.
    pub method: &'static str,
    /// Resident bytes with the server up and no idle connections.
    pub baseline_bytes: Option<u64>,
    /// Resident bytes right after the last idle connection was established.
    pub connected_bytes: Option<u64>,
    /// Resident bytes at the end of the hold.
    pub held_bytes: Option<u64>,
    /// The larger of the two growths over baseline.
    pub growth_bytes: Option<u64>,
    /// Growth per idle connection.
    pub per_connection_bytes: Option<u64>,
    /// The budget for this mix: HTTP connections and WebSockets at their own ceilings.
    pub budget_bytes: u64,
}

/// Descriptors of this process (both ends of every connection live in it).
#[derive(Debug, Clone, Serialize)]
pub struct Descriptors {
    /// Open before the idle connections.
    pub baseline: Option<u64>,
    /// Open with all idle connections held.
    pub held: Option<u64>,
    /// The soft limit the run was checked against.
    pub limit: Option<u64>,
}

/// Opening the connections.
#[derive(Debug, Clone, Serialize)]
pub struct Accept {
    /// Milliseconds from the first connect until the last connection was established and, for
    /// HTTP and WebSocket, had completed its first exchange.
    pub time_to_last_ms: u64,
    /// Connections established.
    pub established: usize,
    /// Client connects or handshakes that failed.
    pub connect_errors: u64,
    /// The first failure, if any.
    pub first_connect_error: Option<String>,
    /// Accept errors the server logged.
    pub accept_errors: u64,
    /// Whether the accept-error counter was live.
    pub accept_errors_counted: bool,
    /// HTTP connections `spot-http` counted open once all were established.
    pub server_open_connections: usize,
    /// WebSocket sessions the realtime bus counted (`spot-http` does not count upgraded
    /// connections).
    pub server_ws_sessions: usize,
}

/// The busy clients, on the empty server and with the idle connections held.
#[derive(Debug, Clone, Serialize)]
pub struct Latency {
    /// Percentiles on the empty server.
    pub empty: Percentiles,
    /// Percentiles with the idle connections held.
    pub loaded: Percentiles,
    /// Failed busy requests, both phases.
    pub busy_errors: u64,
    /// The p99 ceiling derived from the empty one, microseconds.
    pub p99_ceiling_us: u64,
}

/// Stopping the server.
#[derive(Debug, Clone, Serialize)]
pub struct Shutdown {
    /// Milliseconds until the server counted no open connection.
    pub server_closed_ms: u64,
    /// Idle client sockets still open after the grace plus tolerance.
    pub clients_still_open: usize,
    /// Tokio tasks alive before the server started.
    pub tasks_before: usize,
    /// Tokio tasks alive with everything held.
    pub tasks_held: usize,
    /// Tokio tasks alive after shutdown.
    pub tasks_after: usize,
}

/// A finished run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// What was asked.
    pub setup: Setup,
    /// Memory.
    pub memory: Memory,
    /// Descriptors.
    pub descriptors: Descriptors,
    /// Opening the connections.
    pub accept: Accept,
    /// Busy-client latency.
    pub latency: Latency,
    /// Shutdown.
    pub shutdown: Shutdown,
    /// The verdict of each budget.
    pub checks: Vec<Check>,
    /// Whether every check passed.
    pub passed: bool,
}

impl Report {
    /// The checks that failed.
    #[must_use]
    pub fn failed(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.ok).collect()
    }

    /// The report as pretty JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

/// What a run produced: a report, or a reason it did not start (a descriptor limit too low).
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The run was carried out.
    Completed(Box<Report>),
    /// The run did not start, and why.
    Skipped(String),
}

impl Outcome {
    /// The outcome as JSON for stdout.
    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            Self::Completed(report) => report.to_json(),
            Self::Skipped(reason) => serde_json::json!({"skipped": reason}).to_string(),
        }
    }
}
