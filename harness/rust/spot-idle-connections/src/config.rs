//! What a run is asked to do (`RunConfig`) and what it must stay under (`Budgets`).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::HarnessError;

/// The share of each connection kind, in whole percent; the three add up to 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mix {
    /// Plain HTTP/1.1 keep-alive connections: one request, then idle.
    pub http1: u32,
    /// HTTP/2 connections (prior knowledge): one request, then idle.
    pub http2: u32,
    /// WebSocket connections that authenticated and joined a room.
    pub websocket: u32,
}

impl Default for Mix {
    fn default() -> Self {
        Self {
            http1: 50,
            http2: 25,
            websocket: 25,
        }
    }
}

impl Mix {
    /// The mix as a pure WebSocket load, for example.
    #[must_use]
    pub fn only_websocket() -> Self {
        Self {
            http1: 0,
            http2: 0,
            websocket: 100,
        }
    }

    /// Checks the three shares add up to 100.
    ///
    /// # Errors
    /// [`HarnessError`] when they do not.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if self.http1 + self.http2 + self.websocket == 100 {
            Ok(())
        } else {
            Err(HarnessError::new(
                "the connection mix must add up to 100 percent",
            ))
        }
    }

    /// How many of `total` connections are of each kind, `(http1, http2, websocket)`. The
    /// rounding remainder goes to HTTP/1.1, so the three always add up to `total`.
    #[must_use]
    pub fn split(&self, total: usize) -> (usize, usize, usize) {
        let share = |pct: u32| total * usize::try_from(pct).unwrap_or(0) / 100;
        let (h2, ws) = (share(self.http2), share(self.websocket));
        (total - h2 - ws, h2, ws)
    }
}

/// One run of the harness.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Idle connections to open (default 10,000).
    pub connections: usize,
    /// The kinds of connection and their shares.
    pub mix: Mix,
    /// How long the idle connections are held while the busy clients run, and how long the
    /// empty-server baseline runs (default 30 s each).
    pub hold: Duration,
    /// Busy clients, each on its own HTTP/1.1 connection (default 50).
    pub busy_clients: usize,
    /// Requests per second over all busy clients together (default 100).
    pub busy_rps: u32,
    /// Connects in flight at once while opening the idle connections.
    pub connect_parallelism: usize,
    /// The server's shutdown grace; every connection must be closed within it.
    pub shutdown_grace: Duration,
    /// The WebSocket heartbeat cadence the server advertises (default 25 s).
    pub ws_heartbeat_seconds: u64,
    /// Descriptors kept free beyond the two per connection a same-process run needs.
    pub descriptor_margin: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            connections: 10_000,
            mix: Mix::default(),
            hold: Duration::from_secs(30),
            busy_clients: 50,
            busy_rps: 100,
            connect_parallelism: 256,
            shutdown_grace: Duration::from_secs(10),
            ws_heartbeat_seconds: 25,
            descriptor_margin: 512,
        }
    }
}

impl RunConfig {
    /// Descriptors the run needs: both ends of every connection live in this process, plus the
    /// busy clients, the listener and the margin.
    #[must_use]
    pub fn descriptors_needed(&self) -> u64 {
        let both_ends = u64::try_from(self.connections.saturating_add(self.busy_clients))
            .unwrap_or(u64::MAX)
            .saturating_mul(2);
        both_ends.saturating_add(self.descriptor_margin)
    }

    /// Checks the settings can run.
    ///
    /// # Errors
    /// [`HarnessError`] naming the first setting that cannot.
    pub fn validate(&self) -> Result<(), HarnessError> {
        self.mix.validate()?;
        if self.connections == 0 || self.busy_clients == 0 || self.busy_rps == 0 {
            return Err(HarnessError::new(
                "connections, busy clients and busy rate must be at least 1",
            ));
        }
        if self.connect_parallelism == 0 || self.hold.is_zero() || self.shutdown_grace.is_zero() {
            return Err(HarnessError::new(
                "parallelism, hold and shutdown grace must be above zero",
            ));
        }
        Ok(())
    }

    /// The default run with values from the environment: `SPOT_IDLE_CONNECTIONS`,
    /// `SPOT_IDLE_HOLD_SECONDS`, `SPOT_IDLE_BUSY_CLIENTS`, `SPOT_IDLE_BUSY_RPS` and
    /// `SPOT_IDLE_MIX` (`http1,http2,websocket` in percent, for example `0,100,0`).
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();
        let number = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let Some(n) = number("SPOT_IDLE_CONNECTIONS").and_then(|n| usize::try_from(n).ok()) {
            config.connections = n;
        }
        if let Some(s) = number("SPOT_IDLE_HOLD_SECONDS") {
            config.hold = Duration::from_secs(s);
        }
        if let Some(n) = number("SPOT_IDLE_BUSY_CLIENTS").and_then(|n| usize::try_from(n).ok()) {
            config.busy_clients = n;
        }
        if let Some(n) = number("SPOT_IDLE_BUSY_RPS").and_then(|n| u32::try_from(n).ok()) {
            config.busy_rps = n;
        }
        if let Ok(mix) = std::env::var("SPOT_IDLE_MIX") {
            let parts: Vec<u32> = mix
                .split(',')
                .filter_map(|p| p.trim().parse().ok())
                .collect();
            if let [http1, http2, websocket] = parts[..] {
                config.mix = Mix {
                    http1,
                    http2,
                    websocket,
                };
            }
        }
        config
    }
}

/// The ceilings a run is judged against; the file is `budgets.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Budgets {
    /// Memory ceilings.
    pub memory: MemoryBudget,
    /// Latency ceilings.
    pub latency: LatencyBudget,
    /// Fault ceilings.
    pub faults: FaultBudget,
}

/// Resident memory per idle connection above the empty-server baseline.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MemoryBudget {
    /// KiB per idle HTTP/1.1 or HTTP/2 connection.
    pub http_idle_kib: u64,
    /// KiB per idle WebSocket connection.
    pub ws_idle_kib: u64,
}

/// The busy-client p99 with idle connections held, against the empty server's.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LatencyBudget {
    /// Multiple of the empty-server p99 that is allowed.
    pub p99_factor: u64,
    /// Milliseconds allowed on top of that multiple.
    pub p99_extra_ms: u64,
}

/// Counts that must stay at or under their ceilings (zero by default).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FaultBudget {
    /// Accept errors the server logged.
    pub accept_errors: u64,
    /// Failed client connects or handshakes.
    pub connect_errors: u64,
    /// Failed busy requests.
    pub busy_errors: u64,
    /// Tokio tasks left alive after shutdown beyond the count before the server started.
    pub leaked_tasks: u64,
}

impl Budgets {
    /// Reads and checks a budgets file.
    ///
    /// # Errors
    /// [`HarnessError`] when the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> Result<Self, HarnessError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| HarnessError::io("reading budgets", &e))?;
        Self::parse(&text)
    }

    /// Parses budgets text.
    ///
    /// # Errors
    /// [`HarnessError`] when the text is not valid budgets TOML.
    pub fn parse(text: &str) -> Result<Self, HarnessError> {
        toml::from_str(text).map_err(|e| HarnessError::new(format!("parsing budgets: {e}")))
    }

    /// The `budgets.toml` shipped next to this crate's manifest.
    ///
    /// # Errors
    /// As [`Budgets::from_file`].
    pub fn shipped() -> Result<Self, HarnessError> {
        Self::from_file(&Path::new(env!("CARGO_MANIFEST_DIR")).join("budgets.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_budgets_are_the_documented_defaults() {
        let b = Budgets::shipped().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((b.memory.http_idle_kib, b.memory.ws_idle_kib), (100, 200));
        assert_eq!((b.latency.p99_factor, b.latency.p99_extra_ms), (2, 20));
        assert_eq!(
            (
                b.faults.accept_errors,
                b.faults.connect_errors,
                b.faults.leaked_tasks
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn malformed_budgets_are_an_error_not_a_default() {
        assert!(Budgets::parse("[memory]\nhttp_idle_kib = 1\n").is_err());
    }

    #[test]
    fn the_mix_splits_exactly() {
        let mix = Mix::default();
        for total in [0, 1, 3, 7, 200, 9_999, 10_000] {
            let (a, b, c) = mix.split(total);
            assert_eq!(a + b + c, total);
        }
        assert_eq!(mix.split(200), (100, 50, 50));
        assert!(
            Mix {
                http1: 1,
                http2: 1,
                websocket: 1
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn descriptors_count_both_ends_of_every_connection() {
        let config = RunConfig {
            connections: 200,
            busy_clients: 10,
            descriptor_margin: 100,
            ..RunConfig::default()
        };
        assert_eq!(config.descriptors_needed(), 520);
        assert!(
            RunConfig {
                connections: 0,
                ..RunConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}
