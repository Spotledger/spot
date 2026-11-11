//! Shared helpers for the real-system tests.
//!
//! Everything here is deliberately small and explicit: how a test learns its prefix, how
//! it refuses to touch anything outside that prefix, how it drives the real seam, how it
//! runs `sc.exe`/`net.exe` with a static argument vector (never a shell string), and how
//! it cleans up on every path.

use std::process::Command;
use std::sync::{Arc, Mutex};

use spot_agent_windows::{
    AgentConfig, SealedResultChannel, Secret, SecretLabel, WinApi, WindowsError,
};
use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};

/// The fixed marker every harness object name starts with.
pub const PREFIX_MARKER: &str = "spot-h-";

/// The unique prefix `run.ps1` injected for this run.
///
/// # Panics
/// When `SPOT_HARNESS_PREFIX` is not set: a test that cannot know its prefix could not
/// tell its own objects from the machine's, so it must not run at all. Run the suite
/// through `run.ps1`, which sets it.
pub fn prefix() -> String {
    let value = std::env::var("SPOT_HARNESS_PREFIX")
        .expect("SPOT_HARNESS_PREFIX must be set by run.ps1; run the suite through run.ps1");
    assert!(
        value.starts_with(PREFIX_MARKER),
        "the harness prefix must start with {PREFIX_MARKER:?}, got {value:?}"
    );
    assert!(
        value.len() >= PREFIX_MARKER.len() + 8,
        "the harness prefix is too short to be unique: {value:?}"
    );
    value
}

/// The name of a throw-away object of `kind`, carrying this run's prefix.
pub fn name(kind: &str) -> String {
    let value = format!("{}-{kind}", prefix());
    assert_ours(&value);
    value
}

/// Refuses to touch an object the suite did not create.
///
/// # Panics
/// When `name` does not carry this run's prefix.
pub fn assert_ours(name: &str) {
    assert!(
        name.starts_with(&prefix()),
        "refusing to touch an object outside the harness prefix: {name:?}"
    );
}

/// The real `WinApi`, as the host agent would use it.
pub fn api() -> Arc<dyn WinApi> {
    Arc::new(spot_agent_windows::real::RealWin::new())
}

/// The guard configuration a stock install uses.
pub fn config() -> AgentConfig {
    AgentConfig::default()
}

/// True when the runner marked the VM as having no reachable update source
/// (`SPOT_HARNESS_OFFLINE=1`).
pub fn offline() -> bool {
    std::env::var("SPOT_HARNESS_OFFLINE").as_deref() == Ok("1")
}

/// Runs `sc.exe` with a static argument vector.
///
/// # Panics
/// When `sc.exe` cannot be started (it is present on every Windows install).
pub fn sc(args: &[&str]) -> std::process::Output {
    Command::new("sc")
        .args(args)
        .output()
        .expect("sc.exe must be on PATH on Windows")
}

/// Runs `net.exe` with a static argument vector.
///
/// # Panics
/// When `net.exe` cannot be started (it is present on every Windows install).
pub fn net(args: &[&str]) -> std::process::Output {
    Command::new("net")
        .args(args)
        .output()
        .expect("net.exe must be on PATH on Windows")
}

/// Builds a validated operation invocation, exactly as the host agent would.
pub fn args(op: &str, params: Vec<(&str, ParamValue)>) -> Validated {
    Validated {
        op: OpId::new(op).expect("a catalogue operation id"),
        params: params
            .into_iter()
            .map(|(name, value)| AcceptedParam {
                name: name.to_owned(),
                ty: ParamType::Bool,
                value,
                from_default: false,
                secret: false,
            })
            .collect(),
    }
}

/// A text parameter value.
pub fn text(value: &str) -> ParamValue {
    ParamValue::Text(value.to_owned())
}

/// A boolean parameter value.
pub fn flag(value: bool) -> ParamValue {
    ParamValue::Bool(value)
}

/// A list-of-text parameter value.
pub fn list(values: &[&str]) -> ParamValue {
    ParamValue::List(values.iter().map(|v| ParamValue::Text((*v).to_owned())).collect())
}

/// Cleanup that runs on the success path and on a panic: the Rust equivalent of a
/// `finally` block.
///
/// Steps run in reverse order on drop, and each is wrapped so one failing step cannot
/// skip the rest.
#[derive(Default)]
pub struct Cleanup {
    steps: Vec<Box<dyn FnOnce()>>,
}

impl Cleanup {
    /// An empty cleanup.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a cleanup step.
    pub fn push(&mut self, step: impl FnOnce() + 'static) {
        self.steps.push(Box::new(step));
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        while let Some(step) = self.steps.pop() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step));
        }
    }
}

/// A sealed result channel that records only which account was delivered to and how long
/// the secret was — never the secret itself.
#[derive(Default)]
pub struct RecordingChannel {
    delivered: Mutex<Vec<(String, usize)>>,
}

impl RecordingChannel {
    /// The accounts a secret was delivered for, in order.
    ///
    /// # Panics
    /// When the internal lock is poisoned (a test panicked while holding it).
    #[must_use]
    pub fn accounts(&self) -> Vec<String> {
        self.delivered
            .lock()
            .expect("the recording channel lock")
            .iter()
            .map(|(account, _)| account.clone())
            .collect()
    }
}

impl SealedResultChannel for RecordingChannel {
    fn deliver(&self, label: &SecretLabel, secret: Secret) -> Result<(), WindowsError> {
        self.delivered
            .lock()
            .map_err(|_| WindowsError::SecretDelivery)?
            .push((label.account.clone(), secret.len()));
        Ok(())
    }
}
