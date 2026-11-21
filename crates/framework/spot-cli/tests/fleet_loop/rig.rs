//! The rig of the fleet-loop test (WP-CTL-01.T10): the real control-plane modules (health,
//! licensing and, with PostgreSQL, usage) assembled by the real `ControlPlane::build`, and the
//! real tenant-host emitters (`services/heartbeat.rs`, `services/licence.rs`,
//! `services/metering.rs`) wired to it through a **link** that carries the host's certificate
//! identity (`CN=spot-host/<tenant>/<host>`) exactly as the host listener's middleware reads it.
//!
//! The link calls the host router in-process (the same `tower` service the TLS listener serves),
//! so the two halves agree or disagree on paths, methods, JSON, limits and status codes exactly as
//! they would on the wire. The TLS handshake and the certificate chain are not part of this rig:
//! the control plane's `skeleton` tests cover the wrong-CA refusals and `spot-egress`'s
//! `tls_options` tests cover the client side (see the open questions of the report).
//!
//! Time is one `ManualClock` shared by the control plane and every site; no test sleeps.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_cli::services::heartbeat::sample::{Scope, Signal, SignalSampler};
use spot_cli::services::heartbeat::{
    HeartbeatPorts, HeartbeatService, HeartbeatShipper, NoJitter, ShipOutcome, outcome_of,
};
use spot_cli::services::licence::renew::{FetchError, Fetched, LicenceFetcher, interpret};
use spot_cli::services::licence::{HostConfig, LicenceHost};
use spot_cli::services::metering::{ShipOutcome as UsageOutcome, UsageShipper};
use spot_control_plane::auth::{KeyVault, sha256_hex};
use spot_control_plane::config::Config;
use spot_control_plane::modules::{health, licensing};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_license::TrustedKeys;
use spot_mgmt_api::ports::{AuditSink, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use time::OffsetDateTime;
use tower::ServiceExt as _;

/// The injected "now" every test starts at (Unix seconds).
pub const NOW: i64 = 1_800_000_000;
/// The licence issuer's seed and key id (a throwaway key).
pub const LICENCE_SEED: [u8; 32] = [3; 32];
pub const LICENCE_KEY_ID: &str = "key-a";
/// The seconds between heartbeats.
pub const INTERVAL: u64 = 15;

static NAMES: AtomicU64 = AtomicU64::new(0);

/// A tenant id no other test or run uses.
pub fn fresh_tenant() -> String {
    format!(
        "fl{}-{}",
        std::process::id(),
        NAMES.fetch_add(1, Ordering::SeqCst)
    )
}

struct MemVault(String);

impl KeyVault for MemVault {
    fn ca_key_pem(&self) -> Result<String, spot_control_plane::auth::CaError> {
        Ok(self.0.clone())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn config_text(admin_key_hex: &str) -> String {
    format!(
        r#"
region = "eu-1"
audience = "cp-eu-1"
administrator_keys = ["{admin_key_hex}"]
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
"#
    )
}

thread_local! {
    static HEALTH: RefCell<Option<Arc<health::HealthState>>> = const { RefCell::new(None) };
    static LICENSING: RefCell<Option<Arc<licensing::LicensingState>>> = const { RefCell::new(None) };
}

fn register_health(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = HEALTH.with(|s| s.borrow().clone()).expect("state set");
    health::register_with(ctx, state);
    Ok(())
}

fn register_licensing(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = LICENSING.with(|s| s.borrow().clone()).expect("state set");
    licensing::register_with(ctx, state);
    Ok(())
}

/// The control plane under test.
pub struct Rig {
    pub plane: ControlPlane,
    pub clock: Arc<ManualClock>,
    signer: SoftwareSigner,
}

impl Rig {
    /// Health and licensing over memory rows; with `pool`, the usage module over PostgreSQL too
    /// (its tables are created by the plane's own migrations).
    pub async fn new(pool: Option<deadpool_postgres::Pool>) -> Arc<Self> {
        let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
        let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
        let clock = Arc::new(ManualClock::new(
            OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        ));
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        HEALTH.with(|s| {
            *s.borrow_mut() = Some(health::memory_state(clock_dyn.clone(), 512, None).unwrap());
        });
        LICENSING.with(|s| {
            *s.borrow_mut() = Some(licensing::memory_state(
                clock_dyn.clone(),
                &LICENCE_SEED,
                LICENCE_KEY_ID,
            ));
        });
        let mut registrations = vec![
            Registration {
                name: "health",
                order: health::ORDER,
                register: register_health,
            },
            Registration {
                name: "licensing",
                order: licensing::ORDER,
                register: register_licensing,
            },
        ];
        if pool.is_some() {
            registrations.push(Registration {
                name: "usage",
                order: spot_control_plane::modules::usage::ORDER,
                register: spot_control_plane::modules::usage::register,
            });
        }
        let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
        let plane = ControlPlane::build(BuildDeps {
            config: Arc::new(config),
            pool: pool.clone(),
            clock: clock_dyn,
            audit,
            nonces: Arc::new(MemoryNonceStore::new()),
            vault: Arc::new(MemVault(KeyPair::generate().unwrap().serialize_pem())),
            registrations,
        })
        .unwrap();
        if let Some(pool) = &pool {
            spot_control_plane::migrate(pool, &plane.migrations)
                .await
                .unwrap();
        }
        Arc::new(Self {
            plane,
            clock,
            signer,
        })
    }

    pub fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    pub fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    /// An operator call as `CN=root` (every tenant). Mutations carry a signed command bound to the
    /// body and path, as the operator middleware requires.
    pub async fn operator(&self, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .extension(PeerCertificate::new("CN=root", "fp-root"));
        if method == "POST" {
            let now = self.now();
            let command = Command::new(
                "cp.test",
                json!({ "body_sha256": sha256_hex(body.as_bytes()), "path": path }),
                "cp-eu-1",
                self.signer.key_id(),
                now - 5,
                now + 300,
            );
            let signed = sign(&command, &self.signer).unwrap();
            let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
            request = request.header("x-spot-command", URL_SAFE_NO_PAD.encode(text.as_bytes()));
        }
        let (status, bytes) = answer(
            &self.plane.operator_router,
            request.body(Body::from(body.to_owned())).unwrap(),
        )
        .await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The tenant's health view and the node `<tenant>:<host>` in it.
    pub async fn node(&self, tenant: &str, host: &str) -> Value {
        let (status, view) = self
            .operator("GET", &format!("/cp/v1/health/tenants/{tenant}"), "")
            .await;
        assert_eq!(status, StatusCode::OK, "fleet loop (heartbeat): {view}");
        view["data"]["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|n| n["id"] == format!("{tenant}:{host}")))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Issues the tenant a licence as the operator does; `ends_at` is Unix seconds.
    pub async fn issue_licence(&self, tenant: &str, ends_at: i64) -> StatusCode {
        let body = json!({
            "tenant_id": tenant,
            "plan": "std",
            "starts_at": rfc3339(self.now()),
            "ends_at": rfc3339(ends_at),
            "entitlements": ["reports"],
        })
        .to_string();
        self.operator("POST", "/cp/v1/licences", &body).await.0
    }

    pub async fn renew_licence(&self, tenant: &str, ends_at: i64) -> StatusCode {
        let body = json!({ "ends_at": rfc3339(ends_at) }).to_string();
        self.operator("POST", &format!("/cp/v1/licences/{tenant}/renew"), &body)
            .await
            .0
    }

    pub async fn revoke_licence(&self, tenant: &str) -> StatusCode {
        let body = json!({ "reason": "test" }).to_string();
        self.operator("POST", &format!("/cp/v1/licences/{tenant}/revoke"), &body)
            .await
            .0
    }
}

pub fn rfc3339(epoch: i64) -> String {
    OffsetDateTime::from_unix_timestamp(epoch)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

async fn answer(router: &Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

// ---- the link: one host's certificate identity and its network -----------------------------

/// One host's connection to the control plane. `down` cuts it: every call then fails like an
/// unreachable network (no response at all).
pub struct Link {
    pub rig: Arc<Rig>,
    pub tenant: String,
    pub host: String,
    pub down: AtomicBool,
    /// The bodies the control plane answered 2xx to, in order (to prove no duplicates).
    pub accepted: Mutex<Vec<Vec<u8>>>,
    pub calls: AtomicU64,
}

impl Link {
    pub fn new(rig: &Arc<Rig>, tenant: &str, host: &str) -> Arc<Self> {
        Arc::new(Self {
            rig: Arc::clone(rig),
            tenant: tenant.to_owned(),
            host: host.to_owned(),
            down: AtomicBool::new(false),
            accepted: Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
        })
    }

    /// One call as this host. `None` when the link is down.
    pub async fn call(&self, method: Method, path: &str, body: Vec<u8>) -> Option<(u16, Vec<u8>)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return None;
        }
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .extension(PeerCertificate::new(
                &format!("CN=spot-host/{}/{}", self.tenant, self.host),
                &format!("fp-{}-{}", self.tenant, self.host),
            ))
            .body(Body::from(body.clone()))
            .unwrap();
        let (status, bytes) = answer(&self.rig.plane.host_router, request).await;
        if status.is_success() {
            self.accepted.lock().unwrap().push(body);
        }
        Some((status.as_u16(), bytes))
    }
}

/// The heartbeat emitter's shipper over a link.
pub struct LinkHeartbeat(pub Arc<Link>);

#[async_trait]
impl HeartbeatShipper for LinkHeartbeat {
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome {
        match self
            .0
            .call(Method::POST, spot_cli::services::heartbeat::ENDPOINT, body)
            .await
        {
            Some((status, _)) => outcome_of(status),
            None => ShipOutcome::Retry,
        }
    }
}

/// The licence client's fetcher over a link.
pub struct LinkLicence(pub Arc<Link>);

#[async_trait]
impl LicenceFetcher for LinkLicence {
    async fn fetch(&self) -> Result<Fetched, FetchError> {
        match self
            .0
            .call(
                Method::GET,
                spot_cli::services::licence::renew::ENDPOINT,
                Vec::new(),
            )
            .await
        {
            Some((status, body)) => interpret(status, &body),
            None => Err(FetchError::Unreachable),
        }
    }
}

/// The metering emitter's shipper over a link.
pub struct LinkUsage(pub Arc<Link>);

#[async_trait]
impl UsageShipper for LinkUsage {
    async fn ship(&self, body: Vec<u8>) -> UsageOutcome {
        match self.0.call(Method::POST, "/cp/v1/host/usage", body).await {
            Some((status, _)) => match status {
                200..=299 => UsageOutcome::Accepted,
                400 | 413 | 422 => UsageOutcome::Refused,
                _ => UsageOutcome::Retry,
            },
            None => UsageOutcome::Retry,
        }
    }
}

// ---- a site: the host side of the loops ----------------------------------------------------

/// A sampler whose value the test sets.
pub struct Knob {
    signal: Signal,
    scope: Scope,
    pub value: Arc<AtomicI64>,
}

impl Knob {
    pub fn new(signal: Signal, scope: Scope, initial: i64) -> Arc<Self> {
        Arc::new(Self {
            signal,
            scope,
            value: Arc::new(AtomicI64::new(initial)),
        })
    }

    pub fn set(&self, value: i64) {
        self.value.store(value, Ordering::SeqCst);
    }
}

impl SignalSampler for Knob {
    fn signal(&self) -> Signal {
        self.signal
    }
    fn scope(&self) -> Scope {
        self.scope
    }
    fn sample(&self) -> Result<i64, String> {
        Ok(self.value.load(Ordering::SeqCst))
    }
}

/// A temporary directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spot_fleet_{tag}_{}_{}",
            std::process::id(),
            NAMES.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The settings of a heartbeat emitter in the rig.
pub fn heartbeat_settings(
    dir: &Path,
    max_batches: usize,
) -> spot_cli::services::heartbeat::Settings {
    spot_cli::services::heartbeat::Settings {
        enabled: true,
        control_plane: "https://cp.example.net:8443".to_owned(),
        host_cert: "unused".to_owned(),
        host_key: "unused".to_owned(),
        ca_file: "unused".to_owned(),
        interval_seconds: INTERVAL,
        node_id: "node-1".to_owned(),
        sample_budget_ms: 500,
        outbox_max_batches: max_batches,
        outbox_dir: dir.join("heartbeat-outbox").display().to_string(),
    }
}

/// The egress allow-list of the rig's site: exactly the control plane.
pub struct OnlyControlPlane;

impl spot_cli::services::heartbeat::EgressAllowList for OnlyControlPlane {
    fn allows(&self, host: &str, port: u16) -> bool {
        host == "cp.example.net" && port == 8443
    }
}

/// A heartbeat emitter for the host behind `link`, with `knobs` as its samplers, on the rig's clock.
pub fn heartbeat(
    rig: &Rig,
    link: &Arc<Link>,
    dir: &Path,
    knobs: &[Arc<Knob>],
    max_batches: usize,
) -> HeartbeatService {
    let samplers: Vec<Arc<dyn SignalSampler>> = knobs
        .iter()
        .map(|k| Arc::clone(k) as Arc<dyn SignalSampler>)
        .collect();
    let ports = Arc::new(HeartbeatPorts {
        samplers,
        shipper: Arc::new(LinkHeartbeat(Arc::clone(link))),
        egress: Arc::new(OnlyControlPlane),
        jitter: Arc::new(NoJitter),
        data_dir: dir.to_path_buf(),
    });
    let clock: Arc<dyn Clock> = rig.clock.clone();
    HeartbeatService::new(ports, heartbeat_settings(dir, max_batches), clock).unwrap()
}

/// The three standard knobs of a healthy host.
pub fn healthy_knobs() -> (Arc<Knob>, Arc<Knob>, Arc<Knob>) {
    (
        Knob::new(Signal::HealthProbe, Scope::Node, 1),
        Knob::new(Signal::ErrorRate, Scope::Tenant, 0),
        Knob::new(Signal::QueueLag, Scope::Tenant, 0),
    )
}

/// The issuer's public key as a trusted-keys file under `dir`.
pub fn trusted_keys_file(dir: &Path) -> PathBuf {
    let public = ed25519_dalek::SigningKey::from_bytes(&LICENCE_SEED)
        .verifying_key()
        .to_bytes();
    let path = dir.join("keys.txt");
    std::fs::write(&path, format!("{LICENCE_KEY_ID} {} 0 -\n", hex(&public))).unwrap();
    path
}

/// A licence client for `tenant` under `dir`, trusting the control plane's issuer key, on the rig's
/// clock.
pub fn licence_host(rig: &Rig, dir: &Path, tenant: &str, required: bool) -> LicenceHost {
    let keys = std::fs::read_to_string(trusted_keys_file(dir)).unwrap();
    let trusted: TrustedKeys =
        spot_cli::services::licence::files::parse_trusted_keys(&keys).unwrap();
    let config = HostConfig::new(
        required,
        tenant,
        dir.join("licence.txt"),
        dir.join("clock.state"),
    );
    let clock: Arc<dyn Clock> = rig.clock.clone();
    LicenceHost::load(config, trusted, clock).unwrap()
}
