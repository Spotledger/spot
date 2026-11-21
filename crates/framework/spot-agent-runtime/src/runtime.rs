//! The runtime service control seam: stop with a drain, and start again.
//!
//! A role change or a profile change restarts the runtime; an upgrade closes the site. Both
//! need the same primitive: stop the service while letting in-flight work finish, then start
//! it again. That primitive lives here as [`RuntimeControl`], so the database, role and
//! upgrade modules compose with it and the host's service modules (systemd, the Windows
//! service wrapper) implement it once.
//!
//! Time is an argument ([`stop`](RuntimeControl::stop) takes the instant and a drain window);
//! the module reads no clock.

use crate::error::{RuntimeError, RuntimeResult};

/// What a drain found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainReport {
    /// Whether every in-flight item finished before the window elapsed.
    pub drained: bool,
    /// How many items were still in flight when the window elapsed.
    pub in_flight: u64,
    /// How long the drain waited, in whole seconds.
    pub waited_secs: u64,
}

/// What stopping the runtime did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopReport {
    /// The drain outcome.
    pub drain: DrainReport,
    /// Whether the service is now stopped.
    pub stopped: bool,
}

/// What starting the runtime did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartReport {
    /// Whether the service is now running.
    pub started: bool,
    /// Human notes (for example that one web worker is still warming up).
    pub notes: Vec<String>,
}

/// Stops and starts the runtime service.
///
/// The host's systemd adapter or the Windows service wrapper implements this once; the
/// database, role, profile and upgrade modules hold it as a trait object.
pub trait RuntimeControl: Send + Sync {
    /// Stops the service, draining in-flight work for at most `drain_secs`.
    ///
    /// # Errors
    /// [`RuntimeError::DrainTimeout`] when the runtime is still busy after the window (the
    /// adapter must then decide whether to stop anyway; this module treats it as a refusal),
    /// or [`RuntimeError::ServiceFailed`] when the stop itself fails.
    fn stop(&self, drain_secs: u64, now: i64) -> RuntimeResult<StopReport>;

    /// Starts the service.
    ///
    /// # Errors
    /// [`RuntimeError::ServiceFailed`] when the start fails.
    fn start(&self) -> RuntimeResult<StartReport>;
}

/// Stops the runtime with a drain, turning a timeout into a refusal.
///
/// # Errors
/// [`RuntimeError::DrainTimeout`] when the service did not drain, or the port's error.
pub fn stop_runtime(
    control: &dyn RuntimeControl,
    drain_secs: u64,
    now: i64,
) -> RuntimeResult<StopReport> {
    let report = control.stop(drain_secs, now)?;
    if !report.drain.drained {
        return Err(RuntimeError::DrainTimeout {
            seconds: drain_secs,
        });
    }
    Ok(report)
}

/// Starts the runtime.
///
/// # Errors
/// The port's error.
pub fn start_runtime(control: &dyn RuntimeControl) -> RuntimeResult<StartReport> {
    control.start()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeControl {
        drained: bool,
        stops: Mutex<usize>,
        starts: Mutex<usize>,
    }

    impl RuntimeControl for FakeControl {
        fn stop(&self, drain_secs: u64, _now: i64) -> RuntimeResult<StopReport> {
            *self.stops.lock().unwrap() += 1;
            Ok(StopReport {
                drain: DrainReport {
                    drained: self.drained,
                    in_flight: if self.drained { 0 } else { 3 },
                    waited_secs: drain_secs,
                },
                stopped: true,
            })
        }

        fn start(&self) -> RuntimeResult<StartReport> {
            *self.starts.lock().unwrap() += 1;
            Ok(StartReport {
                started: true,
                notes: Vec::new(),
            })
        }
    }

    #[test]
    fn a_drained_stop_proceeds() {
        let control = FakeControl {
            drained: true,
            ..FakeControl::default()
        };
        let report = stop_runtime(&control, 30, 1_000).unwrap();
        assert!(report.stopped);
        assert!(report.drain.drained);
        assert_eq!(*control.stops.lock().unwrap(), 1);
    }

    #[test]
    fn a_failed_drain_is_a_refusal() {
        let control = FakeControl::default();
        let error = stop_runtime(&control, 10, 1_000).unwrap_err();
        assert_eq!(error.code(), crate::error::DRAIN_TIMEOUT);
    }

    #[test]
    fn start_delegates() {
        let control = FakeControl::default();
        assert!(start_runtime(&control).unwrap().started);
        assert_eq!(*control.starts.lock().unwrap(), 1);
    }
}
