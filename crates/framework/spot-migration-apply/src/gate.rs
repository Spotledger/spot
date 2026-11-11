//! The maintenance gate: how users are held out while a run is unverified.
//!
//! The gate is the single switch that admits users. The engine closes it **before** the first
//! step and opens it **only** after verification passed or the old version was restored. While it
//! is closed, new sessions and writes are refused with a [`MaintenanceProblem`]; in-flight
//! transactions are drained with a deadline. The gate's state is journaled, and a restart
//! re-closes it, so a crash never opens the site.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::error::Result;

/// The platform's planned-maintenance problem code.
pub const MAINTENANCE_CODE: &str = "server.maintenance";

/// The state of the maintenance gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "gate")]
pub enum GateStatus {
    /// The site admits users.
    Open,
    /// The site is closed for a migration.
    Closed {
        /// Why the site is closed.
        reason: String,
        /// Unix microseconds the gate closed at.
        since: i64,
    },
}

impl GateStatus {
    /// Whether the gate is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

/// The problem document a refused session or write receives while the site is closed.
///
/// It is the `server.maintenance` problem: the client retries after the window. It never carries
/// a secret or a raw SQL string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceProblem {
    /// The stable problem code (`server.maintenance`).
    pub code: String,
    /// A short title.
    pub title: String,
    /// The reason the site is closed.
    pub detail: String,
    /// Unix microseconds the gate closed at.
    pub since: i64,
    /// When the site is expected back, if known.
    pub retry_after: Option<i64>,
}

impl MaintenanceProblem {
    /// The problem for a closed gate.
    #[must_use]
    pub fn closed(reason: impl Into<String>, since: i64) -> Self {
        Self {
            code: MAINTENANCE_CODE.to_owned(),
            title: "The site is in maintenance".to_owned(),
            detail: reason.into(),
            since,
            retry_after: None,
        }
    }

    /// The same problem with an expected return time.
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: i64) -> Self {
        self.retry_after = Some(retry_after);
        self
    }
}

/// What a drain did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainReport {
    /// How many in-flight transactions finished while the drain waited.
    pub drained: u64,
    /// Whether the deadline passed with work still in flight.
    pub timed_out: bool,
    /// How long the drain waited, in microseconds.
    pub waited_micros: i64,
}

/// The admission gate every session and write consults.
pub trait AdmissionGate: Send + Sync {
    /// Closes the gate: new sessions and writes are refused from now on.
    ///
    /// # Errors
    /// A backend error when the gate cannot be closed; the engine then does not proceed.
    fn close<'a>(&'a self, reason: &'a str, now: i64) -> BoxFuture<'a, Result<()>>;

    /// Opens the gate.
    ///
    /// # Errors
    /// A backend error when the gate cannot be opened.
    fn open<'a>(&'a self, now: i64) -> BoxFuture<'a, Result<()>>;

    /// The gate's state.
    fn status(&self) -> GateStatus;

    /// Waits up to `deadline_micros` for in-flight transactions to finish.
    ///
    /// # Errors
    /// A backend error when the drain cannot be observed.
    fn drain(&self, deadline_micros: i64, now: i64) -> BoxFuture<'_, Result<DrainReport>>;

    /// The problem document a refused session or write receives, or `None` when the gate is open.
    fn problem(&self) -> Option<MaintenanceProblem> {
        match self.status() {
            GateStatus::Open => None,
            GateStatus::Closed { reason, since } => Some(MaintenanceProblem::closed(reason, since)),
        }
    }
}

/// The reference in-memory gate.
///
/// It starts **open** (a fresh process with no journal has nothing to hold out), and the engine
/// closes it before it touches anything. `set_inflight` (the `testing` feature) stands in for the
/// connection pool's in-flight count so the drain's deadline path is exercised without a database.
#[derive(Debug)]
pub struct InMemoryAdmissionGate {
    status: Mutex<GateStatus>,
    inflight: AtomicU64,
    poll_micros: i64,
}

impl Default for InMemoryAdmissionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryAdmissionGate {
    /// A gate that is open and polls the in-flight count every millisecond.
    #[must_use]
    pub fn new() -> Self {
        Self {
            status: Mutex::new(GateStatus::Open),
            inflight: AtomicU64::new(0),
            poll_micros: 1_000,
        }
    }

    /// The same gate with a different drain poll interval, in microseconds.
    #[must_use]
    pub fn with_poll_micros(mut self, poll_micros: i64) -> Self {
        self.poll_micros = poll_micros.max(1);
        self
    }

    /// Sets the in-flight transaction count the drain observes.
    #[cfg(feature = "testing")]
    pub fn set_inflight(&self, inflight: u64) {
        self.inflight.store(inflight, Ordering::SeqCst);
    }
}

impl AdmissionGate for InMemoryAdmissionGate {
    fn close<'a>(&'a self, reason: &'a str, now: i64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self
                .status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = GateStatus::Closed {
                reason: reason.to_owned(),
                since: now,
            };
            Ok(())
        })
    }

    fn open<'a>(&'a self, _now: i64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self
                .status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = GateStatus::Open;
            Ok(())
        })
    }

    fn status(&self) -> GateStatus {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn drain(&self, deadline_micros: i64, _now: i64) -> BoxFuture<'_, Result<DrainReport>> {
        Box::pin(async move {
            let initial = self.inflight.load(Ordering::SeqCst);
            let mut waited = 0i64;
            loop {
                let now_inflight = self.inflight.load(Ordering::SeqCst);
                if now_inflight == 0 {
                    return Ok(DrainReport {
                        drained: initial,
                        timed_out: false,
                        waited_micros: waited,
                    });
                }
                if waited >= deadline_micros {
                    return Ok(DrainReport {
                        drained: initial.saturating_sub(now_inflight),
                        timed_out: true,
                        waited_micros: waited,
                    });
                }
                let step = self.poll_micros.min(deadline_micros - waited).max(1);
                tokio::time::sleep(Duration::from_micros(
                    u64::try_from(step).unwrap_or(u64::MAX),
                ))
                .await;
                waited += step;
            }
        })
    }
}
