//! The service seam of the agent's own update: [`AgentService`] realises
//! `spot_updater::ServiceControl` for a process that *is* the service it updates.
//!
//! The engine calls `stop`, `start(version)`, `wait_ready(deadline)` and `is_healthy`. The
//! agent cannot stop itself and then start the new version, so:
//!
//! - `stop` pauses command intake ([`Quiesce`]) and returns; the process keeps running;
//! - `start(v)` for the version that is running just resumes intake (this is also the
//!   rollback path: the old version never went away); for any other version it launches the
//!   new binary in probe mode through [`Probe`];
//! - `wait_ready(deadline)` waits for the probe to report healthy until `deadline` (Unix
//!   seconds), then aborts it and answers a health error, which makes the engine roll back;
//! - `is_healthy` is the probe's verdict, or `true` when no probe is running.
//!
//! A probe is a short run of the new binary that reaches the gateway over the pinned
//! channel, sends a heartbeat carrying its own version and exits 0. [`ProcessProbe`] spawns
//! `<install root>/versions/<version>/<exe> <args...>`; the `probe` subcommand it needs is an
//! open question in the report (the command-line file is outside this task's paths).
//!
//! Time here is read through an injected [`Clock`], only to compare with the deadline the
//! engine computed from its own `now`.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use semver::Version;
use spot_core::Clock;
use spot_updater::{Layout, ServiceControl, UpdaterError};

use super::error::UpdateError;

/// How the agent pauses and resumes taking commands while an update is in flight.
pub trait Quiesce: Send + Sync {
    /// Stop taking new commands (running ones finish).
    fn pause(&self);
    /// Take commands again.
    fn resume(&self);
}

/// A [`Quiesce`] that is one shared flag the intake loop reads.
#[derive(Debug, Default)]
pub struct QuiesceFlag {
    paused: AtomicBool,
}

impl QuiesceFlag {
    /// A flag that is not paused.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether intake is paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

impl Quiesce for QuiesceFlag {
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
}

/// Where a probe stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeState {
    /// Still running.
    Running,
    /// Finished and reported healthy.
    Healthy,
    /// Failed to start, exited non-zero, or was aborted (a short reason, no output).
    Failed(String),
}

/// A short trial run of a new version.
pub trait Probe: Send + Sync {
    /// Launches the probe of `version`, returning at once.
    ///
    /// # Errors
    /// [`UpdateError`] when it cannot be launched (the binary is missing or not a file).
    fn start(&self, version: &Version) -> Result<(), UpdateError>;

    /// The probe's state now.
    fn state(&self) -> ProbeState;

    /// Kills the probe if it still runs.
    fn abort(&self);
}

/// The probe that runs the new version's own binary.
pub struct ProcessProbe {
    layout: Layout,
    exe: String,
    args: Vec<String>,
    child: Mutex<Option<Child>>,
}

impl ProcessProbe {
    /// A probe that runs `<root>/versions/<version>/<exe> <args...>`; `exe` is a relative
    /// path inside the version directory (for example `spot-agent`).
    #[must_use]
    pub fn new(layout: Layout, exe: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            layout,
            exe: exe.into(),
            args,
            child: Mutex::new(None),
        }
    }

    fn exe_path(&self, version: &Version) -> PathBuf {
        self.layout.version_file(version, &self.exe)
    }
}

impl Probe for ProcessProbe {
    fn start(&self, version: &Version) -> Result<(), UpdateError> {
        let path = self.exe_path(version);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            _ => {
                return Err(UpdateError::Layout(
                    "the new binary is not in place".to_owned(),
                ));
            }
        }
        let child = Command::new(&path)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| UpdateError::Layout("the new binary cannot be started".to_owned()))?;
        let mut slot = self
            .child
            .lock()
            .map_err(|_| UpdateError::Layout("probe state poisoned".to_owned()))?;
        if let Some(mut old) = slot.replace(child) {
            let _ = old.kill();
            let _ = old.wait();
        }
        Ok(())
    }

    fn state(&self) -> ProbeState {
        let Ok(mut slot) = self.child.lock() else {
            return ProbeState::Failed("probe state poisoned".to_owned());
        };
        let Some(child) = slot.as_mut() else {
            return ProbeState::Failed("no probe was started".to_owned());
        };
        match child.try_wait() {
            Ok(None) => ProbeState::Running,
            Ok(Some(status)) if status.success() => ProbeState::Healthy,
            Ok(Some(_)) => ProbeState::Failed("the probe exited with an error".to_owned()),
            Err(_) => ProbeState::Failed("the probe cannot be observed".to_owned()),
        }
    }

    fn abort(&self) {
        if let Ok(mut slot) = self.child.lock()
            && let Some(mut child) = slot.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Restarts the agent into whatever `current` names, after a commit.
pub trait Restarter: Send + Sync {
    /// Asks the service manager for a restart; the process is expected to end soon after.
    ///
    /// # Errors
    /// [`UpdateError::Layout`] when the request cannot be issued.
    fn restart(&self) -> Result<(), UpdateError>;
}

/// A restart through the platform's service manager, as a fixed command line.
#[derive(Clone, Debug)]
pub struct SystemRestarter {
    program: String,
    args: Vec<String>,
}

impl SystemRestarter {
    /// A restarter that runs `program args...` without waiting for it.
    #[must_use]
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    /// The default for this platform: `systemctl --no-block restart spot-agent.service` on
    /// Linux; on Windows a `sc.exe` stop and start through `cmd.exe`.
    #[must_use]
    pub fn platform_default() -> Self {
        #[cfg(windows)]
        {
            Self::new(
                "cmd.exe",
                vec![
                    "/C".to_owned(),
                    "sc.exe stop SpotAgent & sc.exe start SpotAgent".to_owned(),
                ],
            )
        }
        #[cfg(not(windows))]
        {
            Self::new(
                "systemctl",
                vec![
                    "--no-block".to_owned(),
                    "restart".to_owned(),
                    "spot-agent.service".to_owned(),
                ],
            )
        }
    }
}

impl Restarter for SystemRestarter {
    fn restart(&self) -> Result<(), UpdateError> {
        Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|_| UpdateError::Layout("the service manager cannot be reached".to_owned()))
    }
}

/// The engine's view of the agent as a service. See the module docs.
pub struct AgentService {
    running: Version,
    quiesce: Arc<dyn Quiesce>,
    probe: Arc<dyn Probe>,
    clock: Arc<dyn Clock>,
    poll: Duration,
    probing: AtomicBool,
}

impl AgentService {
    /// The service for the process running `running`.
    #[must_use]
    pub fn new(
        running: Version,
        quiesce: Arc<dyn Quiesce>,
        probe: Arc<dyn Probe>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            running,
            quiesce,
            probe,
            clock,
            poll: Duration::from_millis(200),
            probing: AtomicBool::new(false),
        }
    }

    /// Sets how often `wait_ready` looks at the probe.
    #[must_use]
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }
}

fn service_error(detail: &str) -> UpdaterError {
    UpdaterError::Service {
        detail: detail.to_owned(),
    }
}

impl ServiceControl for AgentService {
    fn stop(&self) -> Result<(), UpdaterError> {
        self.quiesce.pause();
        Ok(())
    }

    fn start(&self, version: &Version) -> Result<(), UpdaterError> {
        if *version == self.running {
            self.probing.store(false, Ordering::SeqCst);
            self.quiesce.resume();
            return Ok(());
        }
        self.probe
            .start(version)
            .map_err(|error| service_error(&error.detail()))?;
        self.probing.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn wait_ready(&self, deadline: i64) -> Result<(), UpdaterError> {
        if !self.probing.load(Ordering::SeqCst) {
            return Ok(());
        }
        loop {
            match self.probe.state() {
                ProbeState::Healthy => return Ok(()),
                ProbeState::Failed(reason) => {
                    return Err(UpdaterError::Health {
                        detail: format!("the new version failed its probe: {reason}"),
                    });
                }
                ProbeState::Running => {}
            }
            if self.now() >= deadline {
                self.probe.abort();
                return Err(UpdaterError::Health {
                    detail: format!("the new version was not healthy by {deadline}"),
                });
            }
            std::thread::sleep(self.poll);
        }
    }

    fn is_healthy(&self) -> Result<bool, UpdaterError> {
        if self.probing.load(Ordering::SeqCst) {
            Ok(self.probe.state() == ProbeState::Healthy)
        } else {
            Ok(true)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use super::*;

    fn script(dir: &std::path::Path, version: &Version, body: &str) -> Layout {
        let layout = Layout::new(dir);
        let path = layout.version_file(version, "spot-agent");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        layout
    }

    fn settle(probe: &ProcessProbe) -> ProbeState {
        let started = Instant::now();
        loop {
            let state = probe.state();
            if state != ProbeState::Running || started.elapsed() > Duration::from_secs(10) {
                return state;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_probe_that_exits_zero_is_healthy_and_one_that_exits_non_zero_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), &Version::parse("2.0.0").unwrap(), "exit 0");
        let probe = ProcessProbe::new(ok, "spot-agent", vec!["probe".to_owned()]);
        probe.start(&Version::parse("2.0.0").unwrap()).unwrap();
        assert_eq!(settle(&probe), ProbeState::Healthy);

        let bad = script(dir.path(), &Version::parse("3.0.0").unwrap(), "exit 3");
        let probe = ProcessProbe::new(bad, "spot-agent", Vec::new());
        probe.start(&Version::parse("3.0.0").unwrap()).unwrap();
        assert!(matches!(settle(&probe), ProbeState::Failed(_)));
    }

    #[test]
    fn a_missing_binary_cannot_be_probed_and_a_hanging_one_can_be_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let probe = ProcessProbe::new(layout, "spot-agent", Vec::new());
        assert!(probe.start(&Version::parse("4.0.0").unwrap()).is_err());

        let hang = script(dir.path(), &Version::parse("5.0.0").unwrap(), "sleep 30");
        let probe = ProcessProbe::new(hang, "spot-agent", Vec::new());
        probe.start(&Version::parse("5.0.0").unwrap()).unwrap();
        assert_eq!(probe.state(), ProbeState::Running);
        probe.abort();
        assert!(matches!(probe.state(), ProbeState::Failed(_)));
    }
}
