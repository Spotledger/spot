//! Shared helpers for the golden-transcript tests: reading a transcript, seeding the fake host
//! from it, and replaying each scenario against the real module.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::Deserialize;
use spot_agent_linux::testing::{FakeSystem, FirewallBackend, State};
use spot_agent_linux::{LinuxContext, LinuxOperation, build_catalogue, modules};
use spot_mgmt_ops::{Invocation, OpId, ParamValue, Validated};

/// One transcript file.
#[derive(Debug, Deserialize)]
pub struct Transcript {
    /// The distribution family (`fedora`, `rhel` or `ubuntu`).
    pub distro: String,
    /// A human description of what the transcript covers.
    pub description: String,
    /// The initial state of the fake host.
    #[serde(default)]
    pub state: StateSeed,
    /// The scenarios to replay.
    pub scenarios: Vec<Scenario>,
}

/// The initial state to seed the fake host with.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct StateSeed {
    /// Installed package names (version `1.0`).
    #[serde(default)]
    pub installed: Vec<String>,
    /// Available updates.
    #[serde(default)]
    pub updates: Vec<UpdateSeed>,
    /// Unit states, by unit name.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSeed>,
    /// Accounts, by name.
    #[serde(default)]
    pub users: BTreeMap<String, u32>,
    /// Whether the package manager lock is held.
    #[serde(default)]
    pub package_locked: bool,
    /// The firewall backend.
    #[serde(default)]
    pub firewall_backend: Option<String>,
    /// Open firewall ports, as `port/protocol`.
    #[serde(default)]
    pub firewall_ports: Vec<String>,
    /// Allowed firewall sources.
    #[serde(default)]
    pub firewall_sources: Vec<String>,
}

/// One seeded update.
#[derive(Debug, Deserialize, Clone)]
pub struct UpdateSeed {
    /// The package name.
    pub name: String,
    /// Whether the update is a security update.
    #[serde(default)]
    pub security: bool,
}

/// One seeded unit state.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct ServiceSeed {
    /// Whether the unit is active.
    #[serde(default)]
    pub active: bool,
    /// Whether the unit is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Whether the unit is failed.
    #[serde(default)]
    pub failed: bool,
}

/// One scenario: an operation, its parameters, and the commands it must emit.
#[derive(Debug, Deserialize)]
pub struct Scenario {
    /// A human name for the scenario.
    pub name: String,
    /// The operation id.
    pub op: String,
    /// The parameters, as JSON.
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
    /// The expected commands, each an argv vector.
    pub expect_argv: Vec<Vec<String>>,
    /// Whether a second plan after apply must be empty.
    #[serde(default)]
    pub idempotent: bool,
}

/// Reads and parses a transcript file under `tests/transcripts/`.
#[must_use]
pub fn read_transcript(distro: &str, name: &str) -> Transcript {
    let path = format!(
        "{}/tests/transcripts/{distro}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {path}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {path}: {error}"))
}

/// Builds the fake host a transcript describes.
#[must_use]
pub fn seed(transcript: &Transcript) -> FakeSystem {
    let system = match transcript.distro.as_str() {
        "fedora" => FakeSystem::fedora(),
        "rhel" => FakeSystem::rhel(),
        "ubuntu" => FakeSystem::ubuntu(),
        other => panic!("unknown distro {other}"),
    };
    let state = transcript.state.clone();
    system.update(|host| apply_seed(host, &state));
    system
}

/// Applies a seed to a state.
pub fn apply_seed(host: &mut State, seed: &StateSeed) {
    host.installed.clear();
    for name in &seed.installed {
        host.installed.insert(name.clone(), "1.0".to_owned());
    }
    host.updates.clear();
    for update in &seed.updates {
        host.updates.push(spot_agent_linux::testing::Update {
            name: update.name.clone(),
            security: update.security,
        });
    }
    for (unit, service) in &seed.services {
        host.services.insert(
            unit.clone(),
            spot_agent_linux::testing::ServiceState {
                active: service.active,
                enabled: service.enabled,
                masked: false,
                failed: service.failed,
            },
        );
    }
    for (name, uid) in &seed.users {
        host.users.insert(
            name.clone(),
            spot_agent_linux::testing::UserState {
                uid: *uid,
                gid: *uid,
                home: format!("/home/{name}"),
                shell: "/bin/bash".to_owned(),
                locked: false,
                groups: std::collections::BTreeSet::new(),
            },
        );
    }
    host.package_locked = seed.package_locked;
    if let Some(backend) = &seed.firewall_backend {
        host.firewall = match backend.as_str() {
            "firewalld" => FirewallBackend::Firewalld,
            "ufw" => FirewallBackend::Ufw,
            "nftables" => FirewallBackend::Nftables,
            other => panic!("unknown firewall backend {other}"),
        };
    }
    host.firewall_ports = seed.firewall_ports.iter().cloned().collect();
    host.firewall_sources = seed.firewall_sources.iter().cloned().collect();
}

/// The context over a fake host.
#[must_use]
pub fn context(system: &FakeSystem) -> std::sync::Arc<LinuxContext> {
    std::sync::Arc::new(LinuxContext::new(system.runner(), system.filesystem()))
}

/// Validates a scenario's parameters into a [`Validated`] invocation.
#[must_use]
pub fn validated(scenario: &Scenario) -> Validated {
    let catalogue = build_catalogue().expect("the catalogue builds");
    let mut invocation = Invocation::new(OpId::new(&scenario.op).expect("a valid op id"));
    for (name, value) in &scenario.params {
        invocation = invocation.with(name.clone(), to_param_value(value));
    }
    catalogue
        .validate(&invocation)
        .unwrap_or_else(|errors| panic!("{}: {errors:?}", scenario.name))
}

/// Converts a JSON value to a [`ParamValue`] (bool, integer, text or list of text).
fn to_param_value(value: &serde_json::Value) -> ParamValue {
    match value {
        serde_json::Value::Bool(flag) => ParamValue::Bool(*flag),
        serde_json::Value::Number(number) => {
            ParamValue::Int(number.as_i64().expect("an integer parameter"))
        }
        serde_json::Value::String(text) => ParamValue::Text(text.clone()),
        serde_json::Value::Array(items) => ParamValue::List(
            items
                .iter()
                .map(|item| ParamValue::Text(item.as_str().expect("a list of text").to_owned()))
                .collect(),
        ),
        other => panic!("unsupported parameter value {other}"),
    }
}

/// The module for `op`.
#[must_use]
pub fn module_for(
    context: &std::sync::Arc<LinuxContext>,
    op: &str,
) -> std::sync::Arc<dyn LinuxOperation> {
    modules(context)
        .into_iter()
        .find(|module| module.op().as_str() == op)
        .unwrap_or_else(|| panic!("no module for {op}"))
}

/// The argv of a command request, as `[program, args…]`.
#[must_use]
pub fn argv(command: &spot_agent_linux::CommandRequest) -> Vec<String> {
    let mut out = vec![command.program.clone()];
    out.extend(command.args.iter().cloned());
    out
}

/// Asserts that `expected` appears as an ordered subsequence of `recorded`.
pub fn assert_subsequence(recorded: &[Vec<String>], expected: &[Vec<String>], scenario: &str) {
    let mut cursor = 0;
    for wanted in expected {
        let found = recorded[cursor..].iter().position(|argv| argv == wanted);
        let Some(offset) = found else {
            panic!(
                "{scenario}: expected command {wanted:?} not found after position {cursor} in {recorded:?}"
            );
        };
        cursor += offset + 1;
    }
}
