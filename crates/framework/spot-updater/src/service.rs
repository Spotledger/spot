//! The service seam: how the update engine stops the running version and starts another.
//!
//! The vocabulary mirrors `spot-pg-supervisor` (`start`, `stop`, `wait_ready`, `is_healthy`).
//! The engine never reads a clock: [`wait_ready`](ServiceControl::wait_ready) takes an absolute
//! Unix-seconds deadline, so a test drives readiness by hand.

use semver::Version;

use crate::error::UpdaterError;

/// Controls the Spotledger service the platform runs as.
///
/// The trait is object-safe; the engine holds `&dyn ServiceControl`. A failed [`stop`] or
/// [`start`] is a typed error, never a panic.
///
/// [`stop`]: ServiceControl::stop
/// [`start`]: ServiceControl::start
pub trait ServiceControl: Send + Sync {
    /// Stops the running service, waiting for it to exit.
    ///
    /// # Errors
    /// [`UpdaterError::Service`] when the service cannot be stopped.
    fn stop(&self) -> Result<(), UpdaterError>;

    /// Starts the service for `version`.
    ///
    /// # Errors
    /// [`UpdaterError::Service`] when the service cannot be started.
    fn start(&self, version: &Version) -> Result<(), UpdaterError>;

    /// Waits until the service is ready to serve, or until `deadline` (Unix seconds).
    ///
    /// # Errors
    /// [`UpdaterError::Health`] when the deadline passes first, [`UpdaterError::Service`] on a
    /// control failure.
    fn wait_ready(&self, deadline: i64) -> Result<(), UpdaterError>;

    /// Whether the service reports itself healthy.
    ///
    /// # Errors
    /// [`UpdaterError::Service`] when the health check itself fails.
    fn is_healthy(&self) -> Result<bool, UpdaterError>;
}
