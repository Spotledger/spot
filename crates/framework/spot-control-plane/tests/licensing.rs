//! The `licensing` module: issue, fetch on the host (offline verification), renewal, revocation,
//! scope and key rotation. Runs over the in-memory rows; no database is needed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::cell::RefCell;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::KeyPair;
use serde_json::{Value, json};
use spot_control_plane::auth::{KeyVault, sha256_hex};
use spot_control_plane::config::Config;
use spot_control_plane::modules::licensing::{self, LicensingState};
use spot_control_plane::registry::Registration;
use spot_control_plane::{BuildDeps, ControlPlane, CpCtx, CpError};
use spot_core::{Clock, ManualClock};
use spot_license::{SignedLicence, TrustedKey, TrustedKeys, verify};
use spot_mgmt_api::ports::{AuditSink, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;
const SEED_A: [u8; 32] = [3; 32];
const SEED_B: [u8; 32] = [4; 32];

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
[[operators]]
subject = "CN=bob"
principal = "bob"
tenants = ["*"]
[[operators]]
subject = "CN=alice"
principal = "alice"
tenants = ["tenant-one"]
"#
    )
}

thread_local! {
    static STATE: RefCell<Option<Arc<LicensingState>>> = const { RefCell::new(None) };
}

fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let state = STATE.with(|s| s.borrow().clone()).expect("state set");
    licensing::register_with(ctx, state);
    Ok(())
}

struct Fixture {
    plane: ControlPlane,
    state: Arc<LicensingState>,
    clock: Arc<ManualClock>,
    signer: SoftwareSigner,
}

fn fixture() -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let state = licensing::memory_state(clock_dyn.clone(), &SEED_A, "key-a");
    STATE.with(|s| *s.borrow_mut() = Some(Arc::clone(&state)));
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let ca = KeyPair::generate().unwrap().serialize_pem();
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock_dyn,
        audit,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca)),
        registrations: vec![Registration {
            name: "licensing",
            order: licensing::ORDER,
            register,
        }],
    })
    .unwrap();
    Fixture {
        plane,
        state,
        clock,
        signer,
    }
}

fn public_key(seed: &[u8; 32]) -> Vec<u8> {
    ed25519_dalek::SigningKey::from_bytes(seed)
        .verifying_key()
        .to_bytes()
        .to_vec()
}

impl Fixture {
    async fn op(&self, who: &str, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .extension(PeerCertificate::new(
                &format!("CN={who}"),
                &format!("fp-{who}"),
            ));
        if method == "POST" {
            let now = self.clock.now_utc().unix_timestamp();
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
            b = b.header("x-spot-command", URL_SAFE_NO_PAD.encode(text.as_bytes()));
        }
        answer(
            &self.plane.operator_router,
            b.body(Body::from(body.to_owned())).unwrap(),
        )
        .await
    }

    async fn host(&self, tenant: &str) -> (StatusCode, Value) {
        let peer = PeerCertificate::new(&format!("CN=spot-host/{tenant}/h1"), "fp-host");
        let req = Request::builder()
            .uri("/cp/v1/host/licence")
            .extension(peer)
            .body(Body::empty())
            .unwrap();
        answer(&self.plane.host_router, req).await
    }

    async fn issue(&self, tenant: &str) -> (StatusCode, Value) {
        let body = json!({ "tenant_id": tenant, "plan": "std", "starts_at": "2026-01-01T00:00:00Z", "ends_at": "2030-01-01T00:00:00Z", "retention_days": 45, "entitlements": ["reports"] }).to_string();
        self.op("root", "POST", "/cp/v1/licences", &body).await
    }
}

async fn answer(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn verified(text: &str, seed: &[u8; 32], key_id: &str) -> spot_license::Licence {
    let signed = SignedLicence::from_text(text).unwrap();
    verify(
        &signed,
        &TrustedKeys::new(vec![
            TrustedKey::from_bytes(key_id, &public_key(seed), 0, None).unwrap(),
        ]),
    )
    .unwrap()
}

#[tokio::test]
async fn issue_then_the_host_verifies_offline_and_only_its_own_licence() {
    let f = fixture();
    let (status, issued) = f.issue("tenant-one").await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    assert_eq!(issued["data"]["status"], "active");
    assert_eq!(issued["data"]["retention_days"], 45);
    let (status, fetched) = f.host("tenant-one").await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    let text = fetched["data"]["licence"].as_str().unwrap();
    let licence = verified(text, &SEED_A, "key-a");
    assert_eq!(
        (licence.tenant.as_deref(), licence.serial, licence.issued_at),
        (Some("tenant-one"), 1, NOW)
    );
    assert_eq!(fetched["data"]["proof"], sha256_hex(text.as_bytes()));
    // Another tenant's host sees nothing of it; the wrong key does not verify it.
    assert_eq!(f.host("tenant-two").await.0, StatusCode::NOT_FOUND);
    let signed = SignedLicence::from_text(text).unwrap();
    let wrong = TrustedKeys::new(vec![
        TrustedKey::from_bytes("key-a", &public_key(&SEED_B), 0, None).unwrap(),
    ]);
    assert!(verify(&signed, &wrong).is_err());
    // Issuing twice is refused.
    assert_eq!(f.issue("tenant-one").await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_tampered_envelope_fails_verification() {
    let f = fixture();
    f.issue("tenant-one").await;
    let text = f.host("tenant-one").await.1["data"]["licence"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut chars: Vec<char> = text.chars().collect();
    let mid = chars.len() / 2;
    chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
    let tampered: String = chars.into_iter().collect();
    let ok = SignedLicence::from_text(&tampered).ok().and_then(|s| {
        verify(
            &s,
            &TrustedKeys::new(vec![
                TrustedKey::from_bytes("key-a", &public_key(&SEED_A), 0, None).unwrap(),
            ]),
        )
        .ok()
    });
    assert!(ok.is_none());
}

#[tokio::test]
async fn renewal_extends_the_validity_keeps_the_entitlements_and_raises_the_serial() {
    let f = fixture();
    f.issue("tenant-one").await;
    f.clock.advance(std::time::Duration::from_secs(3600));
    let body = json!({ "ends_at": "2031-01-01T00:00:00Z" }).to_string();
    let (status, renewed) = f
        .op("root", "POST", "/cp/v1/licences/tenant-one/renew", &body)
        .await;
    assert_eq!(status, StatusCode::OK, "{renewed}");
    assert_eq!(renewed["data"]["ends_at"], "2031-01-01T00:00:00Z");
    assert_eq!(renewed["data"]["entitlements"], json!(["reports"]));
    let text = f.host("tenant-one").await.1["data"]["licence"]
        .as_str()
        .unwrap()
        .to_owned();
    let licence = verified(&text, &SEED_A, "key-a");
    assert_eq!((licence.serial, licence.issued_at), (2, NOW + 3600));
    assert_eq!(licence.entitlements.features, vec!["reports".to_owned()]);
}

#[tokio::test]
async fn revocation_is_listed_and_the_host_stops_receiving_the_licence() {
    let f = fixture();
    f.issue("tenant-one").await;
    let (status, revoked) = f
        .op(
            "root",
            "POST",
            "/cp/v1/licences/tenant-one/revoke",
            &json!({ "reason": "non-payment" }).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["data"]["status"], "revoked");
    let (_, list) = f.op("root", "GET", "/cp/v1/licences", "").await;
    assert_eq!(list["data"][0]["status"], "revoked");
    assert_eq!(f.host("tenant-one").await.0, StatusCode::NOT_FOUND);
    // A revoked licence cannot be renewed.
    let body = json!({ "ends_at": "2031-01-01T00:00:00Z" }).to_string();
    assert_eq!(
        f.op("root", "POST", "/cp/v1/licences/tenant-one/renew", &body)
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn scope_limits_what_an_operator_can_issue_list_and_revoke() {
    let f = fixture();
    f.issue("tenant-one").await;
    f.issue("tenant-two").await;
    let (_, list) = f.op("alice", "GET", "/cp/v1/licences", "").await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let (status, body) = f
        .op(
            "alice",
            "POST",
            "/cp/v1/licences/tenant-two/revoke",
            &json!({ "reason": "x" }).to_string(),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
}

#[tokio::test]
async fn key_rotation_keeps_old_licences_valid_and_signs_new_ones_with_the_new_key() {
    let f = fixture();
    f.issue("tenant-one").await;
    f.state.rotate(&SEED_B, "key-b").unwrap();
    f.issue("tenant-two").await;
    let old = f.host("tenant-one").await.1["data"]["licence"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = f.host("tenant-two").await.1["data"]["licence"]
        .as_str()
        .unwrap()
        .to_owned();
    let both = TrustedKeys::new(vec![
        TrustedKey::from_bytes("key-a", &public_key(&SEED_A), 0, None).unwrap(),
        TrustedKey::from_bytes("key-b", &public_key(&SEED_B), 0, None).unwrap(),
    ]);
    assert!(verify(&SignedLicence::from_text(&old).unwrap(), &both).is_ok());
    assert!(verify(&SignedLicence::from_text(&new).unwrap(), &both).is_ok());
    assert_eq!(SignedLicence::from_text(&new).unwrap().key_id(), "key-b");
}
