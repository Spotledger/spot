//! WP-ADM-02.T5 tests: the admin-key policy follows what enrolment found. A hardware-backed
//! identity store keeps the strict policy; a host without one refuses to enrol unless the
//! operator passes `--allow-software-keys`; the choice is stored in `trust.toml`, shown in
//! the audit log, and only enrolment can set it (a rotation or a command cannot loosen it).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer as _, SigningKey};
use spot_agent::cli::{Command as Cli, parse};
use spot_agent::config::LogLevel;
use spot_agent::enrol::{
    EnrolGateway, EnrolInput, EnrolRequestWire, EnrolResponseWire, PlatformWire, enrol,
    enrol_with_store, rotate_key,
};
use spot_agent::error::AgentError;
use spot_agent::keystore::KeyStoreKind;
use spot_agent::logging::Logger;
use spot_agent::run::{Shutdown, load_runtime};
use spot_agent::state::StateDir;
use spot_agent::trust::TrustFile;
use spot_agent_core::{
    Agent, CommandEnvelope, DelayOnly, EnvelopeSignature, MemoryResultLog, Refusal,
};
use spot_agent_enroll::keys::{MemoryKeyStore, Protection};
use spot_mgmt_ops::{CatalogueBuilder, OpId, OperationSpec, Risk, TargetKind};
use spot_signed_command::{Command, KeyId, MemoryNonceStore};

const TOKEN: &str = "ENROL-TOKEN-7f3a9c2e41b8d605";
const TOKEN_2: &str = "ENROL-TOKEN-second-0000aaaa";
const AGENT: &str = "agent-0001";

fn admin() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// A gateway honouring the contract: a token works once; the anchor is the admin key.
struct FakeGateway {
    used: Mutex<HashSet<String>>,
}

impl FakeGateway {
    fn new() -> Self {
        Self {
            used: Mutex::new(HashSet::new()),
        }
    }
    fn spent(&self) -> usize {
        self.used.lock().unwrap().len()
    }
}

impl EnrolGateway for FakeGateway {
    fn enrol(&self, request: &EnrolRequestWire) -> Result<EnrolResponseWire, AgentError> {
        if !self.used.lock().unwrap().insert(request.token.clone()) {
            return Err(AgentError::TokenInvalid);
        }
        Ok(EnrolResponseWire {
            agent_id: AGENT.to_owned(),
            trust_anchor: STANDARD.encode(admin().verifying_key().to_bytes()),
            poll_after_ms: 1000,
        })
    }
}

#[derive(Clone, Default)]
struct Sink;

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn quiet() -> Logger {
    Logger::new(LogLevel::Debug, Box::new(Sink))
}

fn platform() -> PlatformWire {
    PlatformWire {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        agent_version: "0.1.0".to_owned(),
    }
}

fn fresh_state() -> (tempfile::TempDir, StateDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path().join("state"));
    (dir, state)
}

fn input<'a>(state: &'a StateDir, token: &'a str, allow_software_keys: bool) -> EnrolInput<'a> {
    EnrolInput {
        state,
        gateway_url: "https://gateway.example.test",
        ca: None,
        token,
        key_store: KeyStoreKind::File,
        platform: platform(),
        now: 1_000,
        allow_software_keys,
    }
}

fn hardware_store() -> Box<MemoryKeyStore> {
    Box::new(MemoryKeyStore::from_seed([9u8; 32], Protection::Hardware))
}

/// An agent verifying against what `trust.toml` says, as `run` builds it.
fn agent_from(state: &StateDir) -> Agent {
    let file = TrustFile::load(&state.trust_path()).unwrap();
    let mut builder = CatalogueBuilder::new();
    builder.add(
        "test.read",
        OperationSpec {
            id: OpId::new("server.info.read").unwrap(),
            title: "server.info.read".to_owned(),
            summary: "Test operation.".to_owned(),
            params: vec![],
            risk: Risk::Read,
            target: TargetKind::Server,
        },
    );
    Agent::new(
        AGENT,
        builder.build().unwrap(),
        file.to_store().unwrap(),
        file.to_policy(),
        Arc::new(MemoryNonceStore::new()),
        Box::new(DelayOnly::new(0)),
        Arc::new(MemoryResultLog::new(AGENT)),
    )
}

fn signed_read(signer: &SigningKey, now: i64) -> CommandEnvelope {
    let command = Command::new(
        "server.info.read",
        serde_json::json!({}),
        AGENT,
        KeyId::from_public_key(&signer.verifying_key().to_bytes()),
        now - 30,
        now + 600,
    );
    let signature = signer.sign(&command.signed_message().unwrap());
    CommandEnvelope::from_parts(
        String::from_utf8(command.canonical_bytes().unwrap()).unwrap(),
        vec![EnvelopeSignature::new(signer.verifying_key().to_bytes(), signature).unwrap()],
    )
    .unwrap()
}

#[tokio::test]
async fn hardware_store_enrols_hardware_and_the_policy_requires_it() {
    let (_d, state) = fresh_state();
    let gw = FakeGateway::new();
    // No flag is needed: the host has a hardware-backed store.
    enrol_with_store(
        &input(&state, TOKEN, false),
        hardware_store(),
        &gw,
        &quiet(),
    )
    .unwrap();
    let file = TrustFile::load(&state.trust_path()).unwrap();
    assert!(!file.software_keys_allowed);
    assert!(file.admin_keys.iter().all(|k| k.protection == "hardware"));
    assert!(file.to_policy().require_hardware_keys);
    let audit = std::fs::read_to_string(state.audit_path()).unwrap();
    assert!(!audit.contains("enrolled_software_keys"));
    // A command signed by the trusted (hardware) administrator verifies.
    let decision = agent_from(&state)
        .handle(&signed_read(&admin(), 2_000), 2_000)
        .await;
    assert_eq!(decision.code(), "execute", "{decision:?}");
}

#[test]
fn no_hardware_without_the_flag_is_refused_before_the_token_is_spent() {
    let (_d, state) = fresh_state();
    let gw = FakeGateway::new();
    let err = enrol(&input(&state, TOKEN, false), &gw, &quiet()).unwrap_err();
    assert_eq!(err.code(), "agent.software_keys_not_allowed");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("--allow-software-keys"));
    assert!(!state.is_enrolled());
    assert!(!state.key_path().exists(), "no key is created on refusal");
    assert_eq!(gw.spent(), 0, "the one-time token is not spent");
    // The same token enrols once the operator accepts software keys.
    enrol(&input(&state, TOKEN, true), &gw, &quiet()).unwrap();
    assert!(state.is_enrolled());
}

#[tokio::test]
async fn no_hardware_with_the_flag_enrols_software_and_commands_verify() {
    let (_d, state) = fresh_state();
    let gw = FakeGateway::new();
    enrol(&input(&state, TOKEN, true), &gw, &quiet()).unwrap();
    let file = TrustFile::load(&state.trust_path()).unwrap();
    assert!(file.software_keys_allowed);
    assert!(file.admin_keys.iter().all(|k| k.protection == "software"));
    assert!(!file.to_policy().require_hardware_keys);
    // The audit log shows that software keys were accepted.
    let audit = std::fs::read_to_string(state.audit_path()).unwrap();
    assert!(audit.contains("enrolled_software_keys"));
    // The runtime uses the stored policy, and a signed command is accepted.
    let rt = load_runtime(&state, quiet(), Shutdown::new()).ok().unwrap();
    assert!(!rt.policy.require_hardware_keys);
    let decision = agent_from(&state)
        .handle(&signed_read(&admin(), 2_000), 2_000)
        .await;
    assert_eq!(decision.code(), "execute", "{decision:?}");
}

#[tokio::test]
async fn a_strict_host_refuses_a_software_signer() {
    // The pre-fix shape: software-recorded keys under the strict policy.
    let (_d, state) = fresh_state();
    state.create().unwrap();
    let key = admin().verifying_key().to_bytes();
    TrustFile::from_keys(&[key])
        .save(&state.trust_path())
        .unwrap();
    let decision = agent_from(&state)
        .handle(&signed_read(&admin(), 2_000), 2_000)
        .await;
    assert!(matches!(
        decision.refusal(),
        Some(Refusal::SoftwareKeyRefused { .. })
    ));
}

#[test]
fn a_rotation_cannot_loosen_the_policy() {
    // Hardware host: a software rotation is refused, the trust file is untouched and no
    // half-created key stays behind.
    let (_d, state) = fresh_state();
    let gw = FakeGateway::new();
    enrol_with_store(
        &input(&state, TOKEN, false),
        hardware_store(),
        &gw,
        &quiet(),
    )
    .unwrap();
    let before = std::fs::read_to_string(state.trust_path()).unwrap();
    let err = rotate_key(&input(&state, TOKEN_2, true), &gw, &quiet()).unwrap_err();
    assert_eq!(err.code(), "agent.software_keys_not_allowed");
    assert_eq!(std::fs::read_to_string(state.trust_path()).unwrap(), before);
    let mut next = state.key_path().into_os_string();
    next.push(".next");
    assert!(!std::path::Path::new(&next).exists());
    assert_eq!(gw.spent(), 1, "the rotation token is not spent");
}

#[test]
fn a_rotation_keeps_the_stored_choice() {
    let (_d, state) = fresh_state();
    let gw = FakeGateway::new();
    enrol(&input(&state, TOKEN, true), &gw, &quiet()).unwrap();
    // The flag on a rotation has no effect either way: the stored choice stays.
    rotate_key(&input(&state, TOKEN_2, false), &gw, &quiet()).unwrap();
    let file = TrustFile::load(&state.trust_path()).unwrap();
    assert!(file.software_keys_allowed);
}

#[test]
fn an_old_trust_file_without_the_field_is_strict() {
    let text = "[[admin_keys]]\npublic_key = \"AAAA\"\nprotection = \"software\"\n";
    let file: TrustFile = toml::from_str(text).unwrap();
    assert!(!file.software_keys_allowed);
    assert!(file.to_policy().require_hardware_keys);
}

#[test]
fn the_flag_is_an_enrol_switch_only() {
    let args = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let no_file = |_: &str| None;
    let with = parse(
        &args("enrol --gateway https://g.test --token ABCDEFGHIJKLMNOPQR --allow-software-keys"),
        &no_file,
    )
    .unwrap();
    assert!(matches!(
        with.command,
        Cli::Enrol {
            allow_software_keys: true,
            ..
        }
    ));
    // Placed before another option, it still takes no value.
    let mid = parse(
        &args("enrol --allow-software-keys --gateway https://g.test --token ABCDEFGHIJKLMNOPQR"),
        &no_file,
    )
    .unwrap();
    assert!(matches!(
        mid.command,
        Cli::Enrol {
            allow_software_keys: true,
            ..
        }
    ));
    let without = parse(
        &args("enrol --gateway https://g.test --token ABCDEFGHIJKLMNOPQR"),
        &no_file,
    )
    .unwrap();
    assert!(matches!(
        without.command,
        Cli::Enrol {
            allow_software_keys: false,
            ..
        }
    ));
    assert!(
        parse(
            &args("rotate-key --token ABCDEFGHIJKLMNOPQR --allow-software-keys"),
            &no_file
        )
        .is_err()
    );
    assert!(parse(&args("run --allow-software-keys"), &no_file).is_err());
}
