//! Events the supervisor publishes for the future service wrapper and diagnostics:
//! state changes with timestamps (from the injected [`Clock`](crate::clock::Clock)),
//! restarts, forced stops and stale-lock cleanups. No secrets ever appear here.

use crate::supervisor::SupervisorState;

/// One thing the supervisor did or observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorEvent {
    /// The published state changed.
    StateChanged {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
        /// The new state.
        state: SupervisorState,
    },
    /// The child exited while not stopping; a restart follows.
    Restart {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
        /// 1-based restart attempt number.
        attempt: u32,
    },
    /// The child was killed after the grace period elapsed.
    ForcedStop {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
    },
    /// A stale `postmaster.pid` was removed before start.
    StaleLockCleaned {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
        /// The pid named by the removed file (0 for an empty file).
        pid: u32,
    },
    /// A live postmaster refused startup (event mirror of the typed error).
    LiveLockRefused {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
        /// The live pid found in the file.
        pid: u32,
    },
    /// The first-start extension gate ran: the extension is installed, or was already
    /// present at an acceptable version (ADR-087 section 4.3).
    ExtensionProvisioned {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
        /// The extension (`postgis`).
        extension: String,
        /// The version installed, or already installed.
        version: String,
        /// The call found it present and changed nothing (every later start).
        already_present: bool,
    },
    /// The first-start extension gate failed. The state is `Failed` and `Ready` was never
    /// published; the reason is the host's text and carries no secret.
    ExtensionProvisionFailed {
        /// Milliseconds since the epoch (from the clock).
        at_ms: u64,
        /// The extension (`postgis`).
        extension: String,
        /// Why the install failed.
        reason: String,
    },
}

/// A broadcast side channel of events; each [`EventStream`] has its own cursor.
///
/// Tokio's broadcast receiver deliberately implements `Sync` (multiconsumer poll)
/// instead of per-instance `Clone`, so this wrapper carries a receiver and exposes
/// awaiting/try APIs; cloning yields a fresh cursor via [`Supervisor::events`].
#[derive(Debug)]
pub struct EventStream {
    pub(crate) rx: tokio::sync::broadcast::Receiver<SupervisorEvent>,
}

impl EventStream {
    /// Waits for the next event.
    ///
    /// # Errors
    /// `tokio::sync::broadcast::error::RecvError` on lag or channel close.
    pub async fn recv(
        &mut self,
    ) -> Result<SupervisorEvent, tokio::sync::broadcast::error::RecvError> {
        self.rx.recv().await
    }

    /// Non-blocking receive of an already-buffered event.
    pub fn try_recv(
        &mut self,
    ) -> Result<SupervisorEvent, tokio::sync::broadcast::error::TryRecvError> {
        self.rx.try_recv()
    }
}
