//! `spot-winsvc upgrade --from-staging DIR` (brief: "used by the updater: stop, switch
//! the pointer (spot-updater layout), run `spot migrate` plan-only check for
//! compatibility, start; on health failure roll back to the previous pointer").
//!
//! The pointer files (`current`, `previous`) are at the paths
//! [`spot_updater::Layout::current_path`]/[`Layout::previous_path`] and hold one
//! [`semver::Version`] as trimmed text — the same format `spot-updater`'s own
//! `UpdateEngine` writes (its `read_pointer`/`write_pointer` are private to that crate;
//! this module reads/writes the same public, documented path and format rather than
//! reimplementing `UpdateEngine::apply`'s full signed-manifest flow, which this entry
//! point's narrower contract does not need — see Open Questions).

use semver::Version;
use spot_updater::Layout;
use std::path::Path;

/// What the host does to run `spot migrate --dry-run`-equivalent compatibility check and
/// to cycle the service around the pointer switch. A real implementation shells out to
/// the installed `spot` binary and the Windows SCM; tests use an in-memory
/// [`fake::FakeCycle`].
pub trait UpgradeHost: Send + Sync {
    /// Runs the plan-only migration check for `target`. `Ok(())` means compatible.
    ///
    /// # Errors
    /// [`crate::error::WinsvcError::Failed`] naming why the plan is not compatible.
    fn check_migration_plan(&self, target: &Version) -> Result<(), crate::error::WinsvcError>;
    /// Stops the running service (PostgreSQL and `spot serve`).
    fn stop(&self) -> Result<(), crate::error::WinsvcError>;
    /// Starts the service for the version the `current` pointer now names.
    fn start(&self) -> Result<(), crate::error::WinsvcError>;
    /// Whether the started service reports healthy (`/api/health/ready`).
    fn is_healthy(&self) -> Result<bool, crate::error::WinsvcError>;
}

/// How an upgrade run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// `target` is now `current` and healthy.
    Applied {
        /// The version now serving.
        version: Version,
    },
    /// The migration plan rejected `target`; nothing on disk changed and the service was
    /// never stopped.
    Refused {
        /// Why the plan was rejected.
        reason: String,
    },
    /// The new version failed its health check; `current` was restored to the version
    /// that was running before, and that version was restarted.
    RolledBack {
        /// The version the host rolled back to (`None` when nothing was installed yet).
        restored: Option<Version>,
    },
}

/// Reads the pointer file at `path` (empty/missing means "no version installed yet").
///
/// # Errors
/// [`crate::error::WinsvcError::Failed`] when the file exists but is not a valid version.
fn read_pointer(path: &Path) -> Result<Option<Version>, crate::error::WinsvcError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            Version::parse(trimmed).map(Some).map_err(|e| {
                crate::error::WinsvcError::failed(format!("invalid pointer file: {e}"))
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(crate::error::WinsvcError::io("reading pointer file", &e)),
    }
}

/// Writes `version` to the pointer file at `path` atomically (temp file + rename, so a
/// crash mid-write never leaves a half-written pointer).
fn write_pointer(path: &Path, version: &Version) -> Result<(), crate::error::WinsvcError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, version.to_string())
        .map_err(|e| crate::error::WinsvcError::io("writing pointer file", &e))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| crate::error::WinsvcError::io("renaming pointer file", &e))
}

/// Removes the pointer file at `path`, treating "already absent" as success.
fn remove_pointer(path: &Path) -> Result<(), crate::error::WinsvcError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::error::WinsvcError::io("removing pointer file", &e)),
    }
}

/// Runs one upgrade: checks the migration plan, stops, switches the pointer, starts,
/// and rolls back to the previous pointer on a failed health check.
///
/// # Errors
/// An I/O failure reading or writing a pointer file. A failed migration plan or a failed
/// health check are reported as [`UpgradeOutcome::Refused`]/[`UpgradeOutcome::RolledBack`]
/// (not an `Err`) because both are expected, handled outcomes of an upgrade attempt.
pub fn run_upgrade(
    layout: &Layout,
    target: &Version,
    host: &dyn UpgradeHost,
) -> Result<UpgradeOutcome, crate::error::WinsvcError> {
    if let Err(err) = host.check_migration_plan(target) {
        return Ok(UpgradeOutcome::Refused {
            reason: err.to_string(),
        });
    }

    let previous = read_pointer(&layout.current_path())?;

    host.stop()?;

    if let Some(previous) = &previous {
        write_pointer(&layout.previous_path(), previous)?;
    }
    write_pointer(&layout.current_path(), target)?;

    host.start()?;

    if host.is_healthy()? {
        return Ok(UpgradeOutcome::Applied {
            version: target.clone(),
        });
    }

    // Roll back: restore `current`, stop the failed start, start the restored version.
    host.stop()?;
    match &previous {
        Some(previous_version) => write_pointer(&layout.current_path(), previous_version)?,
        None => remove_pointer(&layout.current_path())?,
    }
    host.start()?;
    Ok(UpgradeOutcome::RolledBack { restored: previous })
}

/// The real [`UpgradeHost`]: runs `spot migrate` (plan-only; brief: "run `spot migrate`
/// plan-only check for compatibility"), controls the Windows service through the SCM
/// (`cfg(windows)`; a non-Windows host reports the SCM itself as the missing
/// prerequisite), and checks `/api/health/ready` with a blocking TCP probe (this
/// function runs outside the Tokio runtime, from `main`, so it is synchronous).
pub struct RealUpgradeHost {
    /// The loaded service config (for the `spot` binary path and the health address).
    pub config: crate::config::ServiceConfig,
}

/// How long [`RealUpgradeHost::is_healthy`] polls `/api/health/ready` before reporting
/// unhealthy. A freshly started `spot serve` needs time to open its listener and answer
/// migrations/warm caches, so one immediate probe (the previous behaviour) reported a
/// perfectly healthy upgrade as failed and rolled it back — see the task's review.
const HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How often [`RealUpgradeHost::is_healthy`] re-probes while polling.
const HEALTH_CHECK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

impl UpgradeHost for RealUpgradeHost {
    fn check_migration_plan(&self, _target: &Version) -> Result<(), crate::error::WinsvcError> {
        let mut command = std::process::Command::new(&self.config.app.spot_binary);
        command.arg("migrate").arg("--dry-run");
        if let Some(site) = &self.config.app.site {
            command.arg("--site").arg(site);
        }
        let output = command
            .output()
            .map_err(|e| crate::error::WinsvcError::io("running spot migrate --dry-run", &e))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(crate::error::WinsvcError::failed(
                "spot migrate reports the staged version is not compatible",
            ))
        }
    }

    fn stop(&self) -> Result<(), crate::error::WinsvcError> {
        #[cfg(windows)]
        {
            crate::service::install::stop()
        }
        #[cfg(not(windows))]
        {
            Err(crate::error::WinsvcError::prerequisite(
                "the Windows Service Control Manager",
            ))
        }
    }

    fn start(&self) -> Result<(), crate::error::WinsvcError> {
        #[cfg(windows)]
        {
            crate::service::install::start()
        }
        #[cfg(not(windows))]
        {
            Err(crate::error::WinsvcError::prerequisite(
                "the Windows Service Control Manager",
            ))
        }
    }

    /// Polls `/api/health/ready` until it answers ready or [`HEALTH_CHECK_TIMEOUT`]
    /// elapses — `crate::service::install::start` already waits for `SERVICE_RUNNING`,
    /// but the application inside the service (migrations, cache warm-up) can take
    /// longer still to answer its own readiness probe, so a single immediate check
    /// (the previous behaviour) would roll back a healthy-but-still-warming-up upgrade.
    fn is_healthy(&self) -> Result<bool, crate::error::WinsvcError> {
        let deadline = std::time::Instant::now() + HEALTH_CHECK_TIMEOUT;
        loop {
            if probe_health_ready(&self.config.app.health_addr)? {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(HEALTH_CHECK_POLL_INTERVAL);
        }
    }
}

/// One blocking `GET /api/health/ready` probe (this function runs outside the Tokio
/// runtime, from `main`, so it is synchronous); a connection failure is treated as "not
/// ready yet", not a hard error, so [`RealUpgradeHost::is_healthy`]'s poll loop can keep
/// retrying while the new version's listener is still coming up.
fn probe_health_ready(health_addr: &str) -> Result<bool, crate::error::WinsvcError> {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect(health_addr) else {
        return Ok(false);
    };
    let request = format!(
        "GET /api/health/ready HTTP/1.1\r\nHost: {health_addr}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Ok(false);
    }
    let mut buf = [0u8; 32];
    let Ok(n) = stream.read(&mut buf) else {
        return Ok(false);
    };
    let status_line = String::from_utf8_lossy(&buf[..n]);
    Ok(status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200"))
}

#[cfg(test)]
/// An in-memory [`UpgradeHost`] for [`run_upgrade`] tests.
pub mod fake {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// An [`UpgradeHost`] whose migration check and health outcome are set by the test.
    pub struct FakeCycle {
        /// When `true`, [`UpgradeHost::check_migration_plan`] returns an error.
        pub refuse_migration: AtomicBool,
        /// Whether [`UpgradeHost::is_healthy`] reports healthy after a start.
        pub healthy_after_start: AtomicBool,
        /// How many times [`UpgradeHost::stop`] was called.
        pub stop_calls: AtomicU32,
        /// How many times [`UpgradeHost::start`] was called.
        pub start_calls: AtomicU32,
        /// The health flag recorded at each [`UpgradeHost::start`] call, in order.
        pub started_healthy_sequence: Mutex<Vec<bool>>,
    }

    impl FakeCycle {
        /// A fake that accepts every migration plan and reports healthy after start.
        #[must_use]
        pub fn new() -> Self {
            FakeCycle {
                refuse_migration: AtomicBool::new(false),
                healthy_after_start: AtomicBool::new(true),
                stop_calls: AtomicU32::new(0),
                start_calls: AtomicU32::new(0),
                started_healthy_sequence: Mutex::new(Vec::new()),
            }
        }
    }

    impl Default for FakeCycle {
        fn default() -> Self {
            Self::new()
        }
    }

    impl UpgradeHost for FakeCycle {
        fn check_migration_plan(&self, _target: &Version) -> Result<(), crate::error::WinsvcError> {
            if self.refuse_migration.load(Ordering::SeqCst) {
                Err(crate::error::WinsvcError::failed(
                    "schema epoch not supported by this build",
                ))
            } else {
                Ok(())
            }
        }

        fn stop(&self) -> Result<(), crate::error::WinsvcError> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn start(&self) -> Result<(), crate::error::WinsvcError> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            let healthy = self.healthy_after_start.load(Ordering::SeqCst);
            self.started_healthy_sequence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(healthy);
            Ok(())
        }

        fn is_healthy(&self) -> Result<bool, crate::error::WinsvcError> {
            Ok(self.healthy_after_start.load(Ordering::SeqCst))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::fake::FakeCycle;
    use super::*;
    use std::sync::atomic::Ordering;
    use tempfile::tempdir;

    fn layout() -> (tempfile::TempDir, Layout) {
        let dir = tempdir().expect("tempdir");
        let layout = Layout::new(dir.path());
        (dir, layout)
    }

    #[test]
    fn a_healthy_upgrade_switches_current_and_keeps_previous() {
        let (_dir, layout) = layout();
        write_pointer(
            &layout.current_path(),
            &Version::parse("1.0.0").expect("test value"),
        )
        .expect("test value");
        let host = FakeCycle::new();

        let outcome = run_upgrade(
            &layout,
            &Version::parse("1.1.0").expect("test value"),
            &host,
        )
        .expect("upgrade runs");

        assert_eq!(
            outcome,
            UpgradeOutcome::Applied {
                version: Version::parse("1.1.0").expect("test value")
            }
        );
        assert_eq!(
            read_pointer(&layout.current_path()).expect("test value"),
            Some(Version::parse("1.1.0").expect("test value"))
        );
        assert_eq!(
            read_pointer(&layout.previous_path()).expect("test value"),
            Some(Version::parse("1.0.0").expect("test value"))
        );
        assert_eq!(host.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.start_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_first_install_has_no_previous_pointer_to_restore() {
        let (_dir, layout) = layout();
        let host = FakeCycle::new();

        let outcome = run_upgrade(
            &layout,
            &Version::parse("1.0.0").expect("test value"),
            &host,
        )
        .expect("upgrade runs");

        assert_eq!(
            outcome,
            UpgradeOutcome::Applied {
                version: Version::parse("1.0.0").expect("test value")
            }
        );
        assert!(!layout.previous_path().exists());
    }

    #[test]
    fn a_refused_migration_plan_never_stops_the_service_or_touches_the_pointer() {
        let (_dir, layout) = layout();
        write_pointer(
            &layout.current_path(),
            &Version::parse("1.0.0").expect("test value"),
        )
        .expect("test value");
        let host = FakeCycle::new();
        host.refuse_migration.store(true, Ordering::SeqCst);

        let outcome = run_upgrade(
            &layout,
            &Version::parse("2.0.0").expect("test value"),
            &host,
        )
        .expect("upgrade runs");

        assert!(matches!(outcome, UpgradeOutcome::Refused { .. }));
        assert_eq!(host.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            read_pointer(&layout.current_path()).expect("test value"),
            Some(Version::parse("1.0.0").expect("test value"))
        );
    }

    #[test]
    fn a_failed_health_check_rolls_back_to_the_previous_version_and_restarts_it() {
        let (_dir, layout) = layout();
        write_pointer(
            &layout.current_path(),
            &Version::parse("1.0.0").expect("test value"),
        )
        .expect("test value");
        let host = FakeCycle::new();
        host.healthy_after_start.store(false, Ordering::SeqCst);

        let outcome = run_upgrade(
            &layout,
            &Version::parse("1.1.0").expect("test value"),
            &host,
        )
        .expect("upgrade runs");

        assert_eq!(
            outcome,
            UpgradeOutcome::RolledBack {
                restored: Some(Version::parse("1.0.0").expect("test value"))
            }
        );
        assert_eq!(
            read_pointer(&layout.current_path()).expect("test value"),
            Some(Version::parse("1.0.0").expect("test value"))
        );
        // stop, start(new, unhealthy), stop, start(restored) = 2 stops, 2 starts.
        assert_eq!(host.stop_calls.load(Ordering::SeqCst), 2);
        assert_eq!(host.start_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_health_check_on_a_first_install_removes_the_pointer_rather_than_restoring_garbage()
    {
        let (_dir, layout) = layout();
        let host = FakeCycle::new();
        host.healthy_after_start.store(false, Ordering::SeqCst);

        let outcome = run_upgrade(
            &layout,
            &Version::parse("1.0.0").expect("test value"),
            &host,
        )
        .expect("upgrade runs");

        assert_eq!(outcome, UpgradeOutcome::RolledBack { restored: None });
        assert_eq!(
            read_pointer(&layout.current_path()).expect("test value"),
            None
        );
    }

    #[test]
    fn pointer_round_trips_through_read_and_write() {
        let (_dir, layout) = layout();
        let version = Version::parse("3.4.5-rc.1").expect("test value");
        write_pointer(&layout.current_path(), &version).expect("test value");
        assert_eq!(
            read_pointer(&layout.current_path()).expect("test value"),
            Some(version)
        );
    }

    #[test]
    fn an_invalid_pointer_file_is_a_failed_error_not_a_panic() {
        let (_dir, layout) = layout();
        std::fs::write(layout.current_path(), "not-a-version").expect("test value");
        let err = read_pointer(&layout.current_path()).expect_err("invalid pointer");
        assert_eq!(err.exit_code(), 3);
    }
}
