//! WP-ADM-02.T2 tests: enrolment against a fake gateway implementing the contract,
//! key and state permissions, strict config, the audit chain, log redaction, the CLI.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use spot_agent::audit::{LocalAuditLog, event_record, verify_file};
use spot_agent::cli::{Command, parse};
use spot_agent::config::{AgentConfig, LogLevel};
use spot_agent::enrol::{
    EnrolGateway, EnrolInput, EnrolRequestWire, EnrolResponseWire, PlatformWire, enrol,
    token_digest_prefix,
};
use spot_agent::error::AgentError;
use spot_agent::keystore::{KeyStoreKind, open_key_store};
use spot_agent::logging::{Logger, build_redactor};
use spot_agent::run::{Shutdown, load_runtime, run};
use spot_agent::state::{StateDir, WINDOWS_STATE_SDDL, windows_icacls_args};
use spot_agent_core::ResultLog as _;

const TOKEN: &str = "ENROL-TOKEN-7f3a9c2e41b8d605";

/// A gateway that honours the contract: a token works once, one answer for every cause.
struct FakeGateway {
    used: Mutex<HashSet<String>>,
    seen_keys: Mutex<Vec<String>>,
}

impl FakeGateway {
    fn new() -> Self {
        Self {
            used: Mutex::new(HashSet::new()),
            seen_keys: Mutex::new(Vec::new()),
        }
    }
}

fn admin_key() -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
        .verifying_key()
        .to_bytes()
}

impl EnrolGateway for FakeGateway {
    fn enrol(&self, request: &EnrolRequestWire) -> Result<EnrolResponseWire, AgentError> {
        if !self.used.lock().unwrap().insert(request.token.clone())
            || !request.token.starts_with("ENROL-")
        {
            return Err(AgentError::TokenInvalid);
        }
        self.seen_keys
            .lock()
            .unwrap()
            .push(request.public_key.clone());
        Ok(EnrolResponseWire {
            agent_id: "agent-0001".to_owned(),
            trust_anchor: STANDARD.encode(admin_key()),
            poll_after_ms: 1000,
        })
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn platform() -> PlatformWire {
    PlatformWire {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        agent_version: "0.1.0".to_owned(),
    }
}

fn do_enrol(
    state: &StateDir,
    gw: &FakeGateway,
    token: &str,
    logger: &Logger,
) -> Result<(), AgentError> {
    enrol(
        &EnrolInput {
            state,
            gateway_url: "https://gateway.example.test",
            ca: None,
            token,
            key_store: KeyStoreKind::File,
            platform: platform(),
            now: 1_000,
            // These tests run on a host without a hardware-backed store.
            allow_software_keys: true,
        },
        gw,
        logger,
    )
    .map(|_| ())
}

fn quiet() -> Logger {
    Logger::new(LogLevel::Debug, Box::new(Capture::default()))
}

fn fresh_state() -> (tempfile::TempDir, StateDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path().join("state"));
    (dir, state)
}

#[test]
fn enrol_writes_state_and_the_token_works_once() {
    let (_d1, s1) = fresh_state();
    let (_d2, s2) = fresh_state();
    let gw = FakeGateway::new();
    do_enrol(&s1, &gw, TOKEN, &quiet()).unwrap();
    assert!(s1.is_enrolled());
    assert!(s1.config_path().exists() && s1.trust_path().exists() && s1.key_path().exists());
    // Second enrolment with the same token, on another host: refused by the gateway.
    let err = do_enrol(&s2, &gw, TOKEN, &quiet()).unwrap_err();
    assert_eq!(err.code(), "agent.token_invalid");
    assert!(
        !s2.is_enrolled(),
        "a refused enrolment leaves no enrolment record"
    );
    // The same state cannot be enrolled twice.
    let err = do_enrol(&s1, &gw, "ENROL-another-token-0000", &quiet()).unwrap_err();
    assert_eq!(err.code(), "agent.already_enrolled");
    // The gateway saw the public key, and the audit log holds the digest prefix only.
    assert_eq!(gw.seen_keys.lock().unwrap().len(), 1);
    let text = std::fs::read_to_string(s1.audit_path()).unwrap();
    assert!(text.contains(&token_digest_prefix(TOKEN)));
    assert!(!text.contains(TOKEN));
    assert_eq!(verify_file(&s1.audit_path(), "agent-0001").unwrap(), 1);
}

#[cfg(unix)]
#[test]
fn modes_are_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, state) = fresh_state();
    do_enrol(&state, &FakeGateway::new(), TOKEN, &quiet()).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(state.root()), 0o700);
    assert_eq!(mode(&state.key_path()), 0o600);
    // A 0644 key file refuses start.
    std::fs::set_permissions(state.key_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = load_runtime(&state, quiet(), Shutdown::new())
        .err()
        .unwrap();
    assert_eq!(err.code(), "agent.state_permissions");
    std::fs::set_permissions(state.key_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(load_runtime(&state, quiet(), Shutdown::new()).is_ok());
}

#[test]
fn config_with_an_unknown_key_is_refused() {
    let err =
        AgentConfig::parse("gateway = \"https://g.example.test\"\nsurprise = 1\n").unwrap_err();
    assert_eq!(err.code(), "agent.config_invalid");
    assert!(AgentConfig::parse("gateway = \"http://g.example.test\"\n").is_err());
    assert!(
        AgentConfig::parse("gateway = \"https://g.example.test\"\npoll_seconds = 0\n").is_err()
    );
    let cfg = AgentConfig::parse(
        "gateway = \"https://g.example.test\"\nallowed_modules = [\"server.service\"]\n",
    )
    .unwrap();
    assert!(cfg.allows("server.service.restart"));
    assert!(!cfg.allows("server.servicex.restart"));
    assert!(
        AgentConfig::parse("gateway = \"https://g.example.test\"")
            .unwrap()
            .allowed_modules
            .is_empty()
    );
    // Start is refused on disk, too.
    let (_d, state) = fresh_state();
    do_enrol(&state, &FakeGateway::new(), TOKEN, &quiet()).unwrap();
    let mut text = std::fs::read_to_string(state.config_path()).unwrap();
    text.push_str("bogus = true\n");
    std::fs::write(state.config_path(), text).unwrap();
    assert_eq!(
        load_runtime(&state, quiet(), Shutdown::new())
            .err()
            .unwrap()
            .code(),
        "agent.config_invalid"
    );
}

#[test]
fn audit_chain_verifies_continues_after_restart_and_detects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    {
        let log = LocalAuditLog::open(&path, "agent-1").unwrap();
        log.append(&event_record("agent-1", "agent.run", "started", None, 10))
            .unwrap();
        log.append(&event_record(
            "agent-1",
            "x.y",
            "refuse",
            Some("agent_core.expired"),
            11,
        ))
        .unwrap();
    }
    {
        let log = LocalAuditLog::open(&path, "agent-1").unwrap();
        assert_eq!(log.head_seq(), 2);
        log.append(&event_record("agent-1", "x.y", "executed", None, 12))
            .unwrap();
    }
    assert_eq!(verify_file(&path, "agent-1").unwrap(), 3);
    let original = std::fs::read_to_string(&path).unwrap();
    let tampered = original.replacen("agent_core.expired", "agent_core.expireD", 1);
    std::fs::write(&path, tampered).unwrap();
    assert_eq!(
        verify_file(&path, "agent-1").unwrap_err().code(),
        "agent.audit_failed"
    );
    // Deleting a middle line breaks the links.
    let lines: Vec<&str> = original.lines().collect();
    std::fs::write(&path, format!("{}\n{}\n", lines[0], lines[2])).unwrap();
    assert!(verify_file(&path, "agent-1").is_err());
}

#[test]
fn audit_rotation_by_size_keeps_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    let log = LocalAuditLog::open_with(&path, "a", 600, 2).unwrap();
    for i in 0..12 {
        log.append(&event_record("a", "x.y", "executed", None, 100 + i))
            .unwrap();
    }
    let rotated = std::fs::read_dir(dir.path()).unwrap().count();
    assert!(
        (2..=3).contains(&rotated),
        "current plus at most two kept files, got {rotated}"
    );
    // A restart continues from the newest file even when the current one is fresh.
    drop(log);
    let reopened = LocalAuditLog::open_with(&path, "a", 600, 2).unwrap();
    assert_eq!(reopened.head_seq(), 12);
    reopened
        .append(&event_record("a", "x.y", "executed", None, 200))
        .unwrap();
    assert_eq!(reopened.head_seq(), 13);
    assert!(verify_file(&path, "a").is_ok());
}

#[test]
fn no_secret_reaches_the_logs() {
    let (_d, state) = fresh_state();
    let capture = Capture::default();
    let logger = Logger::with_redactor(
        LogLevel::Debug,
        Box::new(capture.clone()),
        build_redactor(&[("enrolment_token", TOKEN)]),
    );
    do_enrol(&state, &FakeGateway::new(), TOKEN, &logger).unwrap();
    // A careless message carrying the token is redacted by the known-secret rule.
    logger.info(
        "probe",
        &format!("token was {TOKEN} and password=hunter2hunter2"),
    );
    let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(!out.contains(TOKEN));
    assert!(!out.contains("hunter2hunter2"));
    assert!(out.contains(&token_digest_prefix(TOKEN)));
    // The private seed never appears anywhere the agent writes except the key file.
    let key_text = std::fs::read_to_string(state.key_path()).unwrap();
    let seed = key_text.split('"').nth(1).unwrap_or("").to_owned();
    assert!(seed.len() >= 40);
    for file in [state.config_path(), state.trust_path(), state.audit_path()] {
        assert!(!std::fs::read_to_string(file).unwrap().contains(&seed));
    }
    assert!(!out.contains(&seed));
}

#[test]
fn tpm_store_is_a_stub() {
    let dir = tempfile::tempdir().unwrap();
    let err = open_key_store(KeyStoreKind::Tpm, &dir.path().join("k"))
        .err()
        .unwrap();
    assert_eq!(err.code(), "agent.key_store_unavailable");
}

#[test]
fn windows_acl_strings() {
    assert_eq!(WINDOWS_STATE_SDDL, "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    assert!(!WINDOWS_STATE_SDDL.contains("WD") && !WINDOWS_STATE_SDDL.contains("BU"));
    let args = windows_icacls_args(r"C:\ProgramData\Spotledger\Agent");
    assert_eq!(args[1], "/inheritance:r");
    assert!(args.iter().any(|a| a.starts_with("*S-1-5-18")));
    assert!(args.iter().any(|a| a.starts_with("*S-1-5-32-544")));
    assert_eq!(args.len(), 5);
}

#[cfg(windows)]
#[test]
fn windows_state_dir_is_created_with_the_acl() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path().join("agent"));
    state.create().unwrap();
    assert!(state.root().exists());
}

#[test]
fn cli_parsing() {
    let args = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let no_file = |_: &str| None;
    let cli = parse(
        &args(
            "--state-dir /x enrol --gateway https://g.test --token ABCDEFGHIJKLMNOPQR --ca ca.pem",
        ),
        &no_file,
    )
    .unwrap();
    assert_eq!(cli.state_dir.as_deref(), Some(std::path::Path::new("/x")));
    assert!(
        matches!(cli.command, Command::Enrol { ref ca, .. } if ca.as_deref() == Some("ca.pem"))
    );
    assert_eq!(parse(&args("run"), &no_file).unwrap().command, Command::Run);
    assert_eq!(
        parse(&args("version"), &no_file).unwrap().command,
        Command::Version
    );
    assert!(parse(&args("run --token X"), &no_file).is_err());
    assert!(parse(&args("enrol --gateway https://g.test"), &no_file).is_err());
    assert!(parse(&args("bogus"), &no_file).is_err());
    let from_file = parse(
        &args("enrol --gateway https://g.test --token-file t"),
        &|_| Some("  TOKENFROMFILE0123456 \n".to_owned()),
    )
    .unwrap();
    assert!(
        matches!(from_file.command, Command::Enrol { ref token, .. } if token == "TOKENFROMFILE0123456")
    );
}

#[tokio::test]
async fn run_starts_idles_and_stops_on_shutdown() {
    let (_d, state) = fresh_state();
    // A pinned CA file that is not there: the trust mode is the one under test, whatever
    // system trust store the host running the test has.
    enrol(
        &EnrolInput {
            state: &state,
            gateway_url: "https://gateway.example.test",
            ca: Some("no-such-ca.pem"),
            token: TOKEN,
            key_store: KeyStoreKind::File,
            platform: platform(),
            now: 1_000,
            allow_software_keys: true,
        },
        &FakeGateway::new(),
        &quiet(),
    )
    .unwrap();
    let capture = Capture::default();
    let logger = Logger::new(LogLevel::Info, Box::new(capture.clone()));
    let shutdown = Shutdown::new();
    let trigger = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        trigger.trigger();
    });
    // Since the outbound channel (WP-ADM-02.T3) `run` serves it: a pinned CA file that cannot
    // be read is refused (the system roots are not a fallback when a CA is named), after
    // logging the start and recording the stop.
    let error = run(&state, logger, shutdown, 2_000).await.unwrap_err();
    assert!(
        error.to_string().contains("cannot read the CA file"),
        "{error}"
    );
    let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("starting"), "{out}");
    // started + stopped were recorded after the enrolment entry; the chain verifies.
    assert_eq!(verify_file(&state.audit_path(), "agent-0001").unwrap(), 3);
}

#[tokio::test]
async fn run_refuses_when_not_enrolled() {
    let (_d, state) = fresh_state();
    let err = run(&state, quiet(), Shutdown::new(), 0).await.unwrap_err();
    assert_eq!(err.code(), "agent.not_enrolled");
}
