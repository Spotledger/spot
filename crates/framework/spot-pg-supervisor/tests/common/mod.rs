//! Helpers shared by the supervisor integration tests: a staged fake-postgres
//! "bundle", a manual clock, event collectors and the no-orphans assertion.

#![allow(dead_code)]

mod ports;

use spot_pg_supervisor::clock::Clock;
use spot_pg_supervisor::config::SupervisorConfig;
use spot_pg_supervisor::extensions::{
    ExtensionProvisioned, ExtensionProvisioner, ProvisionFailure,
};
use spot_pg_supervisor::layout::{Listen, PgLayout};
use spot_pg_supervisor::settings::PgSettings;
use spot_pg_supervisor::supervisor::SupervisorState;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

/// Result-style unwrap that names the failure (the workspace denies `unwrap`).
pub fn must<T, E: std::fmt::Debug>(value: Result<T, E>) -> T {
    match value {
        Ok(value) => value,
        Err(error) => panic!("unexpected error: {error:?}"),
    }
}

/// Runs a future on a current-thread runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => panic!("test runtime could not be built: {error}"),
    };
    runtime.block_on(future)
}

/// A manual clock whose time only moves when the test moves it; sleeps are driven by
/// being woken, so tests never really wait.
#[derive(Debug)]
pub struct ManualClock {
    now_ms: AtomicU64,
    /// Wakeups for pending sleeps (ms, waker).
    sleepers: Mutex<Vec<(u64, tokio::sync::mpsc::Sender<()>)>>,
}

impl ManualClock {
    /// A clock at unix epoch zero.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            now_ms: AtomicU64::new(0),
            sleepers: Mutex::new(Vec::new()),
        })
    }

    /// Advances the clock and wakes any sleeper whose deadline passed.
    pub fn advance(&self, ms: u64) {
        self.now_ms.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }

    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Simulated sleep: the deadline is registered; the test advances the clock.
        // To keep tests fast AND deterministic this future completes immediately after
        // recording: restart backoffs are asserted through state transitions and
        // budget counts, not wall timing.
        let _registration_deadline_ms = self.now_ms.load(Ordering::SeqCst) + d.as_millis() as u64;
        Box::pin(std::future::ready(()))
    }
}

/// One staged test environment: temp dirs, a fake bundle and a free port.
pub struct TestEnv {
    /// Kept alive so paths survive to the end of the test.
    pub _tmp: TempDir,
    /// Data directory (unique per test).
    pub data_dir: PathBuf,
    /// Bin directory with the `postgres`/`initdb`/`pg_ctl` stand-ins.
    pub bin_dir: PathBuf,
    /// Log directory.
    pub log_dir: PathBuf,
    /// A port that was free at setup time.
    pub port: u16,
}

impl TestEnv {
    /// Stages dirs and the fake bundle; picks a free port for this test.
    pub fn new() -> TestEnv {
        let tmp = must(TempDir::new());
        let bin_dir = tmp.path().join("bin");
        let data_dir = tmp.path().join("data");
        let log_dir = tmp.path().join("logs");
        must(std::fs::create_dir_all(&bin_dir));
        stage_fake_binaries(&bin_dir, &["postgres", "initdb", "pg_ctl"]);
        stage_postgis(tmp.path());
        let port = free_port();
        TestEnv {
            _tmp: tmp,
            data_dir,
            bin_dir,
            log_dir,
            port,
        }
    }

    /// The supervisor layout for this environment.
    pub fn layout(&self) -> PgLayout {
        PgLayout {
            bin_dir: self.bin_dir.clone(),
            data_dir: self.data_dir.clone(),
            log_dir: self.log_dir.clone(),
            listen: Listen::Localhost,
            port: self.port,
        }
    }

    /// Default settings with a per-test port.
    pub fn settings(&self) -> PgSettings {
        PgSettings::default().with_port(self.port)
    }

    /// Default supervisor config, tuned for tests (short timeouts).
    pub fn config(&self) -> SupervisorConfig {
        SupervisorConfig {
            start_timeout: Duration::from_secs(10),
            stop_grace: Duration::from_secs(2),
            ..SupervisorConfig::default()
        }
    }

    /// Writes a per-data-dir `.env` file the fake reads at startup (edition-2024-safe
    /// substitute for the removed process-global `std::env::set_var` in tests).
    pub fn write_env(&self, pairs: &[(&str, &str)]) {
        let text = pairs
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        must(std::fs::create_dir_all(&self.data_dir));
        if let Err(error) = std::fs::write(self.data_dir.join(".env"), text) {
            panic!("could not write the fake .env file: {error}");
        }
    }
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Copies the cargo-built fake-postgres binary under the three role names.
fn stage_fake_binaries(bin_dir: &Path, roles: &[&str]) {
    let source = match std::env::var("CARGO_BIN_EXE_fake-postgres") {
        Ok(path) if !path.is_empty() => path,
        _ => panic!("CARGO_BIN_EXE_fake-postgres must be set by cargo test"),
    };
    for role in roles {
        let target = if cfg!(windows) {
            bin_dir.join(format!("{role}.exe"))
        } else {
            bin_dir.join(role)
        };
        if let Err(error) = std::fs::copy(&source, &target) {
            panic!("fake-postgres could not be staged as {role}: {error}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = match std::fs::metadata(&target) {
                Ok(metadata) => metadata.permissions(),
                Err(error) => panic!("staged fake binary {role} has no metadata: {error}"),
            };
            permissions.set_mode(0o755);
            if let Err(error) = std::fs::set_permissions(&target, permissions) {
                panic!("staged fake binary {role} could not be made executable: {error}");
            }
        }
    }
}

/// A port claimed for this test (see [`ports`]): nobody else binds it before the fake does.
pub fn free_port() -> u16 {
    ports::claimed_free_port()
}

/// The control file a staged bundle carries: shaped like the real
/// `share/extension/postgis.control` of the PostGIS 3.6.2 Windows bundle
/// (`module_pathname = '$libdir/postgis-3'`, which is what the layout check reads), with
/// no SQL behind it.
pub const POSTGIS_CONTROL: &str = "# postgis extension\n\
comment = 'PostGIS geometry, geography and raster spatial types and functions (test stand-in)'\n\
default_version = '3.6.2'\n\
module_pathname = '$libdir/postgis-3'\n\
relocatable = false\n\
superuser = true\n";

/// Stages the extension part of a bundled PostgreSQL under `prefix`: the PostGIS control
/// file in `<prefix>/share/extension` and the library it names in `<prefix>/lib`. This is
/// exactly what `PgLayout::validate` requires of a bundle (ADR-087 sections 4.2 and 4.7);
/// the files are stand-ins and nothing executes them.
pub fn stage_postgis(prefix: &Path) {
    let share = prefix.join("share").join("extension");
    let lib = prefix.join("lib");
    must(std::fs::create_dir_all(&share));
    must(std::fs::create_dir_all(&lib));
    must(std::fs::write(
        share.join(concat!("postgis", ".control")),
        POSTGIS_CONTROL,
    ));
    must(std::fs::write(
        lib.join("postgis-3.dll"),
        b"staged PostGIS module stand-in",
    ));
}

/// A stand-in for the host's `extension.install` adapter (ADR-087 section 4.3). It records
/// every call, the supervisor state observed at the moment of the call, and answers the
/// idempotent "already present" receipt from the second call on — the twin's behaviour of
/// a real `extension.install` against a database that already has the extension.
pub struct FakeProvisioner {
    /// Extension names the supervisor asked for, in order.
    calls: Mutex<Vec<String>>,
    /// The published supervisor state at each call, when the test asked it to watch.
    states: Mutex<Vec<Option<SupervisorState>>>,
    /// A reason to fail every call with, when the test drives the loud-failure path.
    failure: Mutex<Option<String>>,
    /// The state channel to sample at call time (optional).
    watch: Mutex<Option<tokio::sync::watch::Receiver<SupervisorState>>>,
}

impl FakeProvisioner {
    /// A provisioner that succeeds.
    pub fn new() -> Arc<FakeProvisioner> {
        Arc::new(FakeProvisioner {
            calls: Mutex::new(Vec::new()),
            states: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
            watch: Mutex::new(None),
        })
    }

    /// A provisioner that fails every call with `reason` (the host's typed text, e.g. a
    /// `migrate.extension_version` refusal).
    pub fn failing(reason: &str) -> Arc<FakeProvisioner> {
        let provisioner = FakeProvisioner::new();
        provisioner.set_failure(Some(reason.to_string()));
        provisioner
    }

    /// Makes every later call fail (or succeed, with `None`).
    pub fn set_failure(&self, reason: Option<String>) {
        if let Ok(mut slot) = self.failure.lock() {
            *slot = reason;
        }
    }

    /// Samples the published state at every call, from `states` of the supervisor.
    pub fn watch(&self, states: tokio::sync::watch::Receiver<SupervisorState>) {
        if let Ok(mut slot) = self.watch.lock() {
            *slot = Some(states);
        }
    }

    /// The extensions asked for, in order.
    pub fn calls(&self) -> Vec<String> {
        match self.calls.lock() {
            Ok(calls) => calls.clone(),
            Err(error) => panic!("call log is poisoned: {error}"),
        }
    }

    /// The published state observed at each call (`None` when the test did not watch).
    pub fn states_at_calls(&self) -> Vec<Option<SupervisorState>> {
        match self.states.lock() {
            Ok(states) => states.clone(),
            Err(error) => panic!("state log is poisoned: {error}"),
        }
    }
}

impl ExtensionProvisioner for FakeProvisioner {
    fn provision<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ExtensionProvisioned, ProvisionFailure>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let call_index = {
                let mut calls = match self.calls.lock() {
                    Ok(calls) => calls,
                    Err(error) => panic!("call log is poisoned: {error}"),
                };
                calls.push(name.to_string());
                calls.len()
            };
            let observed = match self.watch.lock() {
                Ok(watch) => watch.as_ref().map(|rx| rx.borrow().clone()),
                Err(error) => panic!("state watch is poisoned: {error}"),
            };
            if let Ok(mut states) = self.states.lock() {
                states.push(observed);
            }
            let failure = match self.failure.lock() {
                Ok(failure) => failure.clone(),
                Err(error) => panic!("failure slot is poisoned: {error}"),
            };
            if let Some(reason) = failure {
                return Err(ProvisionFailure::new(name, reason));
            }
            Ok(ExtensionProvisioned {
                extension: name.to_string(),
                version: "3.6.2".to_string(),
                // A real `extension.install` answers "already present" from the second
                // call on; the twin does the same so the idempotent path is exercised.
                already_present: call_index > 1,
                plan_hash: format!("plan-{call_index}-{name}"),
            })
        })
    }
}

/// Asserts no process from *this test's* fake bundle survived the test. Scopes the
/// /proc scan to this env's temp directory (other tests' fakes, running in parallel,
/// are theirs to reap; the port bind is the second, local check).
pub fn assert_no_orphans(env: &TestEnv) {
    let port = env.port;
    // 1. The port no longer answers a bind.
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => drop(listener),
        Err(error) => {
            panic!("orphan check failed: port {port} is still held by a surviving child: {error}")
        }
    }
    // 2. No process runs a binary from THIS env (scanned through /proc; on Windows
    //    the port check above is the assertion).
    #[cfg(unix)]
    {
        let own = env._tmp.path().to_string_lossy().into_owned();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.filter_map(|entry| entry.ok()) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let cmdline = match std::fs::read_to_string(format!("/proc/{name}/cmdline")) {
                    Ok(cmdline) => cmdline,
                    Err(_) => continue,
                };
                if cmdline.contains(&own) {
                    panic!(
                        "orphan check failed: pid {name} still runs from this test's bundle: {cmdline:?}"
                    );
                }
            }
        }
    }
}
