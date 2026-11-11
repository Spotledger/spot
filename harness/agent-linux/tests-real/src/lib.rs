//! Test-support helpers for the `spot-agent-linux-real` conformance crate (WP-ADM-04.T3).
//!
//! The tests in `tests/real.rs` run the module handlers against a genuine distribution
//! inside a container or a VM. This module holds the small amount of shared glue: a unique
//! name for every throwaway object, a part-1 context over the real seams, an invocation
//! validated through the part-1 catalogue, and a cleanup guard so a test that fails still
//! removes what it created.
//!
//! This is test-support code, so `unwrap`/`expect` are allowed here: a helper that cannot
//! do its job should fail the test loudly, not return an error the test then has to
//! re-check.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_agent_linux::real::{StdCommandRunner, StdFileSystem};
use spot_agent_linux::{CommandRunner, FileSystem, LinuxContext, LinuxOperation};
use spot_mgmt_ops::{Invocation, OpId, ParamValue, Validated};

/// The prefix every throwaway object this suite creates carries. The tests assert they
/// only ever touch objects with this prefix.
pub const PREFIX: &str = "spotreal";

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique, lower-case alphanumeric name with the suite's prefix, valid as an account,
/// group, snapshot or LVM name. The process id and a per-process counter make it unique
/// across tests in one run; the run's fresh container makes it unique across runs. No wall
/// clock is read.
#[must_use]
pub fn unique_name(kind: &str) -> String {
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let value = (u64::from(std::process::id()) << 16) | (sequence & 0xFFFF);
    format!("{PREFIX}{kind}{value:x}")
}

/// The distribution family the machine reports, from `/etc/os-release` (`fedora`, `rhel`,
/// `ubuntu` or the raw `ID`).
#[must_use]
pub fn family() -> String {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let id = os_release_field(&text, "ID").to_lowercase();
    let id_like = os_release_field(&text, "ID_LIKE").to_lowercase();
    for token in id.split_whitespace().chain(id_like.split_whitespace()) {
        match token {
            "fedora" => return "fedora".to_owned(),
            "rhel" | "centos" | "rocky" | "almalinux" | "oracle" => return "rhel".to_owned(),
            "ubuntu" => return "ubuntu".to_owned(),
            "debian" => return "debian".to_owned(),
            _ => {}
        }
    }
    id
}

/// The `ID=` or `ID_LIKE=` value of an `/etc/os-release` document, quotes stripped.
fn os_release_field(text: &str, key: &str) -> String {
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key {
            return value.trim().trim_matches('"').to_owned();
        }
    }
    String::new()
}

/// Refuses to continue unless this machine is a container or a VM the harness owns.
///
/// The tests create accounts, mount loop devices and read the real firewall, so running
/// `cargo test -- --ignored` on a developer's host must never get that far. A container
/// runtime marker (`/.dockerenv`, `/run/.containerenv`) or an explicit
/// `SPOT_HARNESS_DISPOSABLE=1` (which `vm/run.sh` sets inside its VM) is required.
///
/// # Panics
/// When none of the markers is present.
pub fn assert_disposable() {
    let marked = std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::env::var("SPOT_HARNESS_DISPOSABLE").is_ok_and(|value| value == "1");
    assert!(
        marked,
        "refusing to run the real-system tests outside a disposable container or VM: \
         use harness/agent-linux/run.sh (or vm/run.sh)"
    );
}

/// A part-1 context over the real command and filesystem seams.
///
/// # Panics
/// When the machine is not a disposable container or VM (see [`assert_disposable`]).
#[must_use]
pub fn part1_context() -> Arc<LinuxContext> {
    assert_disposable();
    let runner: Arc<dyn CommandRunner> = Arc::new(StdCommandRunner::new());
    let fs: Arc<dyn FileSystem> = Arc::new(StdFileSystem::new());
    Arc::new(LinuxContext::new(runner, fs))
}

/// The part-1 module that implements `op`.
///
/// # Panics
/// When no part-1 module implements the operation.
#[must_use]
pub fn part1_module(context: &Arc<LinuxContext>, op: &str) -> Arc<dyn LinuxOperation> {
    spot_agent_linux::modules(context)
        .into_iter()
        .find(|module| module.op().as_str() == op)
        .unwrap_or_else(|| panic!("no part-1 module implements {op}"))
}

/// A validated invocation of a part-1 operation, through the real catalogue.
///
/// # Panics
/// When the operation is unknown or a parameter fails validation — both are test bugs.
#[must_use]
pub fn validate(op: &str, params: Vec<(&str, ParamValue)>) -> Validated {
    let catalogue = spot_agent_linux::build_catalogue().expect("the part-1 catalogue builds");
    let mut invocation = Invocation::new(OpId::new(op).expect("a valid operation id"));
    for (name, value) in params {
        invocation = invocation.with(name, value);
    }
    catalogue
        .validate(&invocation)
        .unwrap_or_else(|errors| panic!("the invocation of {op} is invalid: {errors:?}"))
}

/// Runs a setup command directly (never through a shell), returning its trimmed stdout.
///
/// # Panics
/// When the machine is not a disposable container or VM, or the program is missing or
/// exits non-zero, with the command's own stderr.
pub fn setup(argv: &[&str]) -> String {
    assert_disposable();
    let (program, args) = argv.split_first().expect("a non-empty argv");
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("could not run {program}: {error}"));
    assert!(
        output.status.success(),
        "setup command {argv:?} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A cleanup guard: every argv pushed is run on drop, in the order it was pushed, and a
/// failure is ignored (a cleanup that cannot run must not turn a passing test red, but the
/// resource leak is visible in the machine's state).
#[derive(Default)]
pub struct Cleanup {
    commands: Vec<Vec<String>>,
}

impl Cleanup {
    /// An empty guard.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedules `argv` to run when the guard drops.
    pub fn push(&mut self, argv: &[&str]) {
        self.commands
            .push(argv.iter().map(|item| (*item).to_owned()).collect());
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for argv in self.commands.drain(..) {
            if let Some((program, args)) = argv.split_first() {
                let _ = Command::new(program).args(args).output();
            }
        }
    }
}
