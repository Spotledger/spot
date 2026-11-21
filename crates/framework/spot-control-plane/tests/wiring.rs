//! The production wiring of the control plane (`WP-CTL-01.T9`): every provider `main` installs,
//! the refusals for a deployment key that is named and unusable, and the readiness of every module
//! with the providers present and with each one removed.
//!
//! The providers are process-wide (`OnceLock`s in the modules), so "remove one registration" cannot
//! run twice in one process: `the_matrix_...` starts this test binary again once per case
//! (`wiring_child`) with the environment of that case, and the child builds the real service over
//! a real PostgreSQL (`SPOT_TEST_PG_URL`, printed as skipped when unset) with fake providers, then
//! reads `/cp/v1/readiness`. The key tests need no database.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rcgen::KeyPair;
use serde_json::Value;
use spot_control_plane::auth::KeyVault;
use spot_control_plane::config::Config;
use spot_control_plane::modules::deploy::AgentLink;
use spot_control_plane::modules::health::NotifySink;
use spot_control_plane::modules::moves::{HostPortInputs, HostPorts, SourceQuarantine, TenantBusy};
use spot_control_plane::modules::rollouts::{StepReport, TenantMigrator};
use spot_control_plane::modules::tenants::ProvisionerInputs;
use spot_control_plane::wiring::{self, Providers};
use spot_control_plane::{BuildDeps, ControlPlane, CpError};
use spot_deploy_wizard::{EnrolledAgents, Host, Receipt, SignedFacts};
use spot_mgmt_api::ports::{AuditSink, BoxFuture as MgmtFuture, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_rehearsal::Fingerprint;
use spot_signed_command::{MemoryNonceStore, SignedCommand, Signer, SoftwareSigner};
use spot_tenancy::{Tenant, TenantId};
use spot_tenancy_adapters::{AdapterError, MigrationRunner};
use spot_tenant_move::{
    BoxFuture, Chunk, FingerprintSource, MemoryMoveStore, MoveId, MovePlan, QuiescePort,
    Result as MoveResult, Side, TransferPort,
};
use tower::ServiceExt as _;

const DEPLOY_KEY_ENV: &str = "SPOT_WIRING_DEPLOY_KEY";
const LICENCE_KEY_ENV: &str = "SPOT_WIRING_LICENCE_KEY";
const PG_URL_ENV: &str = "SPOT_WIRING_PG_URL";
const CHILD_ENV: &str = "SPOT_WIRING_CHILD";
const SKIP_ENV: &str = "SPOT_WIRING_SKIP";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn admin_signer() -> SoftwareSigner {
    SoftwareSigner::from_seed(&[9u8; 32]).unwrap()
}

fn config_text(wiring_section: &str) -> String {
    format!(
        r#"
region = "eu-1"
audience = "cp-eu-1"
administrator_keys = ["{admin}"]
{wiring_section}
[database]
host = "127.0.0.1"
dbname = "control"
user = "cp"
password_env = "SPOT_CP_TEST_PASSWORD_UNSET"
tls = false
[operator_listener]
listen = "127.0.0.1:0"
server_cert = "/nonexistent/op.pem"
server_key = "/nonexistent/op.key"
client_ca = "/nonexistent/operator-ca.pem"
[host_listener]
listen = "127.0.0.1:1"
server_cert = "/nonexistent/host.pem"
server_key = "/nonexistent/host.key"
client_ca = "/nonexistent/host-ca.pem"
[host_ca]
cert = "/nonexistent/host-ca.pem"
key = "/nonexistent/host-ca.key"
common_name = "Spot host CA"
[[operators]]
subject = "CN=root"
principal = "root"
tenants = ["*"]
"#,
        admin = hex(&admin_signer().public_key_bytes()),
    )
}

fn config_with_key_env(name: &str) -> Config {
    Config::from_toml(&config_text(&format!(
        "[wiring]\ndeployment_key_env = \"{name}\"\n"
    )))
    .unwrap()
}

// ------------------------------------------------------------------------------------ the key

#[test]
fn a_named_deployment_key_that_is_unset_or_empty_refuses_the_start() {
    let config = config_with_key_env("SPOT_X_KEY");
    for env in [None, Some(String::new())] {
        let result = wiring::deployment_signer(&config, &|_| env.clone());
        let error = result.err().expect("the start is refused").to_string();
        assert!(
            error.contains("cp.wiring.deployment_key_missing"),
            "{error}"
        );
    }
    // The same through `install`: nothing is installed for a bad key.
    let result = wiring::install(&config, Providers::default(), &|_| None);
    assert!(result.is_err());
}

#[test]
fn a_deployment_key_that_is_not_a_key_refuses_the_start_without_echoing_it() {
    let config = config_with_key_env("SPOT_X_KEY");
    for bad in ["not-hex", &"0".repeat(63), &"g".repeat(64)] {
        let text = bad.to_owned();
        let error = wiring::deployment_signer(&config, &|_| Some(text.clone()))
            .err()
            .expect("the start is refused")
            .to_string();
        assert!(
            error.contains("cp.wiring.deployment_key_invalid"),
            "{error}"
        );
        assert!(!error.contains(bad), "the key text is in the error");
    }
}

#[test]
fn the_deployment_key_must_not_be_an_administrator_key() {
    // The administrator key list holds the PUBLIC half of seed [9; 32]; a deployment seed whose
    // public half is the same is refused. (The seed itself is a different value than the list.)
    let config = config_with_key_env("SPOT_X_KEY");
    let seed = hex(&[9u8; 32]);
    let error = wiring::deployment_signer(&config, &|_| Some(seed.clone()))
        .err()
        .expect("the start is refused")
        .to_string();
    assert!(
        error.contains("cp.wiring.deployment_key_not_separate"),
        "{error}"
    );
}

#[test]
fn a_usable_deployment_key_gives_a_signer_and_no_section_gives_none() {
    let config = config_with_key_env("SPOT_X_KEY");
    let seed = hex(&[7u8; 32]);
    let signer = wiring::deployment_signer(&config, &|_| Some(seed.clone()))
        .unwrap()
        .expect("a signer");
    let expected = SoftwareSigner::from_seed(&[7u8; 32]).unwrap();
    assert_eq!(signer.key_id(), expected.key_id());
    let none = Config::from_toml(&config_text("")).unwrap();
    assert!(
        wiring::deployment_signer(&none, &|_| Some("ignored".into()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_configuration_refuses_an_empty_variable_name_and_unknown_wiring_keys() {
    let empty = config_text("[wiring]\ndeployment_key_env = \"\"\n");
    assert!(Config::from_toml(&empty).is_err());
    let unknown = config_text("[wiring]\ndeployment_key = \"abc\"\n");
    assert!(Config::from_toml(&unknown).is_err());
}

#[test]
fn install_without_providers_lists_what_is_missing_and_wires_nothing_it_cannot() {
    let config = Config::from_toml(&config_text("")).unwrap();
    let wired = wiring::install(&config, Providers::default(), &|_| None).unwrap();
    let unwired: BTreeSet<&str> = wired.unwired.iter().map(|(module, _)| *module).collect();
    for module in ["tenants", "moves", "rollouts", "deploy", "cloud_backup"] {
        assert!(
            unwired.contains(module),
            "{module} is not listed: {wired:?}"
        );
    }
    for module in ["health", "desired_state", "licensing"] {
        assert!(wired.wired.contains(&module), "{module}: {wired:?}");
    }
}

#[test]
fn the_service_table_is_the_module_table_with_the_same_names_and_orders() {
    let wired = wiring::registrations();
    let modules = spot_control_plane::modules::REGISTRY;
    assert_eq!(wired.len(), modules.len());
    for (a, b) in wired.iter().zip(modules) {
        assert_eq!((a.name, a.order), (b.name, b.order));
    }
    // The orders the brief names.
    let order = |name: &str| wired.iter().find(|r| r.name == name).map(|r| r.order);
    assert_eq!(order("tenants"), Some(20));
    assert_eq!(order("licensing"), Some(30));
    assert_eq!(order("health"), Some(160));
    assert_eq!(order("deploy"), Some(170));
    assert_eq!(order("desired_state"), Some(180));
    assert_eq!(order("cloud_backup"), Some(190));
}

// -------------------------------------------------------------------------------- the fakes

struct Runner;
impl MigrationRunner for Runner {
    fn ensure_schema(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Ok(())
    }
    fn drop_schema(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Ok(())
    }
    fn apply_baseline(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Ok(())
    }
    fn seed(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Ok(())
    }
    fn unseed(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Ok(())
    }
}

fn runner_factory(_inputs: ProvisionerInputs) -> Result<Arc<dyn MigrationRunner>, CpError> {
    Ok(Arc::new(Runner))
}

/// Every host-facing port of a move, doing nothing: the readiness only needs them built.
struct Quiet;
impl QuiescePort for Quiet {
    fn quiesce<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _move_id: &'a MoveId,
        _now: u64,
    ) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn resume<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _move_id: &'a MoveId,
        _now: u64,
    ) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl TransferPort for Quiet {
    fn export_chunk<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _after: Option<u64>,
    ) -> BoxFuture<'a, MoveResult<Option<Chunk>>> {
        Box::pin(async { Ok(None) })
    }
    fn import_chunk<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _chunk: &'a Chunk,
    ) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn finish_import<'a>(&'a self, _plan: &'a MovePlan) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn cleanup<'a>(&'a self, _plan: &'a MovePlan) -> BoxFuture<'a, MoveResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl FingerprintSource for Quiet {
    fn measure<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _side: Side,
    ) -> BoxFuture<'a, MoveResult<Fingerprint>> {
        Box::pin(async { Ok(Fingerprint::new(Vec::new())) })
    }
}
impl TenantBusy for Quiet {
    fn busy<'a>(
        &'a self,
        _tenant: &'a TenantId,
    ) -> MgmtFuture<'a, Result<Option<&'static str>, CpError>> {
        Box::pin(async { Ok(None) })
    }
}
impl SourceQuarantine for Quiet {
    fn keep_read_only<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _move_id: &'a MoveId,
        _until_ms: u64,
    ) -> MgmtFuture<'a, Result<(), &'static str>> {
        Box::pin(async { Ok(()) })
    }
}

fn host_ports_factory(_inputs: HostPortInputs) -> Result<HostPorts, CpError> {
    let quiet = Arc::new(Quiet);
    Ok(HostPorts {
        moves: Arc::new(MemoryMoveStore::new()),
        quiesce: quiet.clone(),
        transfer: quiet.clone(),
        fingerprints: quiet.clone(),
        busy: quiet.clone(),
        quarantine: quiet,
    })
}

struct Hosts;
impl TenantMigrator for Hosts {
    fn run_step<'a>(
        &'a self,
        _tenant: &'a str,
        _release: &'a str,
        _epoch: u64,
        _step: &'static str,
    ) -> MgmtFuture<'a, Result<StepReport, String>> {
        Box::pin(async { Err("the fake host is not reachable".to_owned()) })
    }
}

struct Directory;
impl EnrolledAgents for Directory {
    fn enrolled_agent(&self, _host: &Host) -> Option<String> {
        None
    }
    fn verify(&self, _agent_id: &str, _message: &[u8], _signature: &[u8]) -> bool {
        false
    }
}

struct Link;
impl AgentLink for Link {
    fn collect_facts<'a>(&'a self, _host: &'a str) -> MgmtFuture<'a, Result<SignedFacts, String>> {
        Box::pin(async { Err("the fake agent is not reachable".to_owned()) })
    }
    fn deliver<'a>(
        &'a self,
        _audience: &'a str,
        _command: &'a SignedCommand,
    ) -> MgmtFuture<'a, Result<Receipt, String>> {
        Box::pin(async { Err("the fake agent is not reachable".to_owned()) })
    }
}

struct Notifier;
impl NotifySink for Notifier {
    fn deliver<'a>(
        &'a self,
        _notification: &'a spot_fleet_health::AlertNotification,
    ) -> MgmtFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

struct MemVault(String);
impl KeyVault for MemVault {
    fn ca_key_pem(&self) -> Result<String, spot_control_plane::auth::CaError> {
        Ok(self.0.clone())
    }
}

// ------------------------------------------------------------------------------ the matrix

/// The cases: (case name, the module that must report not ready, or `None`).
const CASES: [(&str, Option<&str>); 9] = [
    ("all", None),
    ("runner_factory", Some("tenants")),
    ("host_ports_factory", Some("moves")),
    ("migrator", Some("rollouts")),
    ("directory", Some("deploy")),
    ("agent_link", Some("deploy")),
    ("backup_dir", Some("cloud_backup")),
    // A deployment key that is named and absent is a refusal to start, not a module that is
    // not ready: the child checks the refusal itself.
    ("deployment_key", None),
    // The notification sink is optional: alerts are kept and shown without it.
    ("notifier", None),
];

#[test]
fn the_matrix_every_module_is_ready_and_removing_one_registration_unreadies_exactly_that_module() {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set (the wiring matrix needs a PostgreSQL)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("spot_cp_wiring_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("archive");
    let backup = dir.join("backup");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::create_dir_all(&backup).unwrap();
    let tenants_config = dir.join("tenants.toml");
    std::fs::write(
        &tenants_config,
        format!(
            "provisioner_url_env = \"{PG_URL_ENV}\"\narchive_root = {archive:?}\npg_bin = \"/usr/bin\"\nlicence_key_env = \"{LICENCE_KEY_ENV}\"\n[[clusters]]\ncluster = \"c1\"\ndatabase = \"d1\"\nregion = \"eu-1\"\ncapacity = 10\nurl_env = \"{PG_URL_ENV}\"\n"
        ),
    )
    .unwrap();
    // The licensing module is mounted only over a seed file (owner-only, hex key).
    let seed = dir.join("licence.seed");
    std::fs::write(&seed, hex(&[9u8; 32])).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&seed, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    for (case, _) in CASES {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "wiring_child", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env(SKIP_ENV, case)
            .env(PG_URL_ENV, &url)
            .env("SPOT_TEST_PG_URL", &url)
            .env(LICENCE_KEY_ENV, hex(&[5u8; 32]))
            .env("SPOT_CP_LICENCE_SEED_FILE", &seed)
            .env("SPOT_CP_TENANTS_CONFIG", &tenants_config);
        if case != "backup_dir" {
            command.env("SPOT_CP_BACKUP_DIR", &backup);
        }
        if case != "deployment_key" {
            command.env(DEPLOY_KEY_ENV, hex(&[7u8; 32]));
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "case {case} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// One case of the matrix, in its own process. Does nothing unless the parent started it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_child() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let case = std::env::var(SKIP_ENV).unwrap();
    let removed = CASES
        .iter()
        .find(|(name, _)| *name == case)
        .map(|(_, module)| *module)
        .unwrap();
    let config = config_with_key_env(DEPLOY_KEY_ENV);
    let env = |name: &str| std::env::var(name).ok();

    if case == "deployment_key" {
        let error = wiring::install(&config, all_providers(), &env)
            .expect_err("a named and missing key refuses the start")
            .to_string();
        assert!(
            error.contains("cp.wiring.deployment_key_missing"),
            "{error}"
        );
        return;
    }

    let mut providers = all_providers();
    match case.as_str() {
        "runner_factory" => providers.runner_factory = None,
        "host_ports_factory" => providers.host_ports_factory = None,
        "migrator" => providers.migrator = None,
        "directory" => providers.directory = None,
        "agent_link" => providers.agent_link = None,
        "notifier" => providers.notifier = None,
        _ => {}
    }
    let wired = wiring::install(&config, providers, &env).unwrap();
    match removed {
        Some(module) => assert!(
            wired.unwired.iter().any(|(name, _)| *name == module),
            "{module} is not listed as unwired: {wired:?}"
        ),
        None => assert!(
            wired.unwired.is_empty(),
            "nothing is missing in case {case}: {wired:?}"
        ),
    }

    let url = std::env::var(PG_URL_ENV).unwrap();
    let backend = spot_postgres::PgBackend::connect(
        spot_postgres::PgConfig::from_url(&url).expect("the test URL parses"),
    )
    .await
    .expect("the test database is reachable");
    let pool = backend.pool();
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: Some(pool.clone()),
        clock: Arc::new(spot_core::SystemClock::new()),
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(KeyPair::generate().unwrap().serialize_pem())),
        registrations: wiring::registrations(),
    })
    .expect("the service is assembled over the wired modules");
    spot_control_plane::migrate(&pool, &plane.migrations)
        .await
        .expect("the control schema is applied");

    let response = plane
        .operator_router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/cp/v1/readiness")
                .extension(PeerCertificate::new("CN=root", "fp-root"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let modules = body["data"]["modules"].as_object().expect("a module map");
    let not_ready: BTreeSet<&str> = modules
        .iter()
        .filter(|(_, ready)| **ready == Value::Bool(false))
        .map(|(name, _)| name.as_str())
        .collect();
    let expected: BTreeSet<&str> = removed.into_iter().collect();
    assert_eq!(
        not_ready, expected,
        "case {case}: the modules that are not ready are not exactly the removed one: {body}"
    );
    assert_eq!(
        status,
        if expected.is_empty() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        "{body}"
    );
    // The mounted modules are in the service: the DDL of each is in the migrations.
    let migrated: BTreeSet<&str> = plane.migrations.iter().map(|m| m.module).collect();
    for module in ["tenants", "health", "desired_state"] {
        assert!(
            migrated.contains(module),
            "{module} has no DDL: {migrated:?}"
        );
    }
    if removed != Some("deploy") {
        assert!(migrated.contains("deploy"), "deploy is not mounted");
    }
    if removed != Some("cloud_backup") {
        assert!(
            migrated.contains("cloud_backup"),
            "cloud_backup is not mounted"
        );
    }
}

fn all_providers() -> Providers {
    Providers {
        runner_factory: Some(runner_factory),
        host_ports_factory: Some(host_ports_factory),
        migrator: Some(Arc::new(Hosts)),
        directory: Some(Arc::new(Directory)),
        agent_link: Some(Arc::new(Link)),
        notifier: Some(Arc::new(Notifier)),
    }
}
