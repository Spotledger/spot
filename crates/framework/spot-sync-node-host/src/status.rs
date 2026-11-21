//! What the operator sees: the link state, the offline-since time and the readiness line.
//!
//! Offline is **normal** for an edge (EDG-01): it never makes the site not-ready. The line says
//! so plainly: `sync_node: connected to hub-1 (lag 0.2s)` or
//! `sync_node: offline 3h12m, 418 changes queued`.

use std::sync::{Arc, Mutex};

/// Where the link is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// No identity or no pin: the loop does not run.
    NotEnrolled,
    /// The loop has not reached the hub yet (start-up).
    Connecting,
    /// The last handshake succeeded and the session is running.
    Connected,
    /// The last session failed; the loop is backing off.
    Offline,
}

/// A copy of the status at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusSnapshot {
    /// The link state.
    pub state: LinkState,
    /// The hub's name for display (never a URL, a fingerprint or a code).
    pub hub: String,
    /// When the link went down, in Unix seconds, while it is [`LinkState::Offline`].
    pub offline_since: Option<i64>,
    /// Changes in the node's outbox not yet acknowledged by the hub.
    pub queued: u64,
    /// Milliseconds since the last round in which both directions were idle (caught up).
    pub lag_ms: Option<u64>,
    /// The code of the last failure.
    pub last_error: Option<&'static str>,
    /// Failed sessions in a row.
    pub failures: u32,
    /// Bundles applied since start.
    pub applied: u64,
    /// Bundles parked, held, refused or quarantined since start.
    pub unsettled: u64,
    /// Changes acknowledged by the hub since start.
    pub pushed: u64,
}

impl StatusSnapshot {
    /// The readiness detail at `now` (Unix seconds).
    pub fn readiness(&self, now: i64) -> String {
        match self.state {
            LinkState::NotEnrolled => "sync_node: not enrolled".to_owned(),
            LinkState::Connecting => "sync_node: connecting".to_owned(),
            LinkState::Connected => {
                let lag = self.lag_ms.unwrap_or(0);
                format!(
                    "sync_node: connected to {} (lag {}.{}s)",
                    self.hub,
                    lag / 1_000,
                    (lag % 1_000) / 100
                )
            }
            LinkState::Offline => {
                let since = self.offline_since.unwrap_or(now);
                format!(
                    "sync_node: offline {}, {} changes queued",
                    span(now.saturating_sub(since)),
                    self.queued
                )
            }
        }
    }
}

/// `3h12m`, `45m`, `2d` from a number of seconds (negative reads as `0m`).
pub fn span(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    if minutes >= 48 * 60 {
        format!("{}d", minutes / (24 * 60))
    } else if minutes >= 60 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

/// The shared, cheap-to-clone status the loop writes and the service reads.
#[derive(Clone, Debug)]
pub struct StatusHandle {
    inner: Arc<Mutex<StatusSnapshot>>,
}

impl StatusHandle {
    /// A status for a hub called `hub`, in the given initial state.
    pub fn new(hub: impl Into<String>, state: LinkState) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StatusSnapshot {
                state,
                hub: hub.into(),
                offline_since: None,
                queued: 0,
                lag_ms: None,
                last_error: None,
                failures: 0,
                applied: 0,
                unsettled: 0,
                pushed: 0,
            })),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut StatusSnapshot) -> R) -> R {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    /// A copy of the status.
    pub fn snapshot(&self) -> StatusSnapshot {
        self.with(|s| s.clone())
    }

    /// The link is connected.
    pub fn connected(&self) {
        self.with(|s| {
            s.state = LinkState::Connected;
            s.offline_since = None;
            s.last_error = None;
            s.failures = 0;
        });
    }

    /// The link is connecting (the loop is about to dial).
    pub fn connecting(&self) {
        self.with(|s| {
            if s.state != LinkState::Offline {
                s.state = LinkState::Connecting;
            }
        });
    }

    /// The session failed with `code` at `now` (Unix seconds). The offline-since time is kept
    /// from the first failure of a run, not moved by the later ones.
    pub fn offline(&self, now: i64, code: &'static str) {
        self.with(|s| {
            if s.offline_since.is_none() {
                s.offline_since = Some(now);
            }
            s.state = LinkState::Offline;
            s.last_error = Some(code);
            s.failures = s.failures.saturating_add(1);
        });
    }

    /// The loop cannot run: no identity.
    pub fn not_enrolled(&self) {
        self.with(|s| s.state = LinkState::NotEnrolled);
    }

    /// Records the unacknowledged outbox size.
    pub fn set_queued(&self, queued: u64) {
        self.with(|s| s.queued = queued);
    }

    /// Records the lag (`None` while catching up).
    pub fn set_lag_ms(&self, lag: Option<u64>) {
        self.with(|s| s.lag_ms = lag);
    }

    /// Adds a pull's counts.
    pub fn add_received(&self, applied: u64, unsettled: u64) {
        self.with(|s| {
            s.applied = s.applied.saturating_add(applied);
            s.unsettled = s.unsettled.saturating_add(unsettled);
        });
    }

    /// Adds pushed changes.
    pub fn add_pushed(&self, pushed: u64) {
        self.with(|s| s.pushed = s.pushed.saturating_add(pushed));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn readiness_reads_as_the_operator_expects() {
        let status = StatusHandle::new("hub-1", LinkState::NotEnrolled);
        assert_eq!(status.snapshot().readiness(0), "sync_node: not enrolled");
        status.connected();
        status.set_lag_ms(Some(200));
        assert_eq!(
            status.snapshot().readiness(0),
            "sync_node: connected to hub-1 (lag 0.2s)"
        );
        status.set_queued(418);
        status.offline(1_000, "sync_http.unavailable");
        status.offline(5_000, "sync_http.unavailable");
        let line = status.snapshot().readiness(1_000 + 3 * 3_600 + 12 * 60);
        assert_eq!(line, "sync_node: offline 3h12m, 418 changes queued");
        status.connected();
        assert_eq!(status.snapshot().offline_since, None);
    }

    #[test]
    fn spans_are_short_and_never_negative() {
        assert_eq!(span(-5), "0m");
        assert_eq!(span(59 * 60), "59m");
        assert_eq!(span(26 * 3_600), "26h00m");
        assert_eq!(span(3 * 24 * 3_600), "3d");
    }
}
