//! The skeleton: configuration, registry, the two listeners, scope, signed mutations, the host CA
//! and the data-contract dependency test. PostgreSQL-backed cases run through `SPOT_TEST_PG_URL`
//! and print why they skip when it is unset.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::{Extension, Json};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use serde_json::{Value, json};
use spot_control_plane::auth::{HostIdentity, KeyVault, OperatorCall, sha256_hex};
use spot_control_plane::config::{Config, ConfigError};
use spot_control_plane::registry::{ModuleRegistry, Registration, RegistryError};
use spot_control_plane::{BuildDeps, ControlPlane, CpCode, CpCtx, CpError, check};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{AuditSink, MemoryAuditSink};
use spot_mgmt_api::tls::PeerCertificate;
use spot_signed_command::{Command, MemoryNonceStore, Signer, SoftwareSigner, sign};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;

struct MemVault(String);

impl KeyVault for MemVault {
    fn ca_key_pem(&self) -> Result<String, spot_control_plane::auth::CaError> {
        Ok(self.0.clone())
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ))
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
subject = "CN=alice"
principal = "alice"
tenants = ["t1"]

[[operators]]
subject = "CN=root"
principal = "root"
tenants = ["*"]
"#
    )
}

fn module_a(ctx: &mut CpCtx) -> Result<(), CpError> {
    #[allow(clippy::result_large_err)]
    async fn tenant(
        Extension(call): Extension<OperatorCall>,
        Path(id): Path<String>,
    ) -> Result<Json<Value>, axum::response::Response> {
        call.scope
            .require(&id)
            .map_err(|error| error.into_response_with(&call.request_id))?;
        Ok(Json(json!({ "data": { "id": id } })))
    }
    async fn create(Extension(call): Extension<OperatorCall>) -> Json<Value> {
        Json(json!({ "data": { "command": call.command.map(|c| c.id) } }))
    }
    async fn usage(Extension(host): Extension<HostIdentity>) -> Json<Value> {
        Json(json!({ "data": { "tenant": host.tenant } }))
    }
    ctx.operator_routes(
        Router::new()
            .route("/cp/v1/tenants/{id}", get(tenant))
            .route("/cp/v1/tenants", post(create)),
    );
    ctx.host_routes(Router::new().route("/cp/v1/host/whoami", get(usage)));
    ctx.add_readiness(Arc::new(|| {
        Box::pin(async { true }) as spot_mgmt_api::ports::BoxFuture<'static, bool>
    }));
    Ok(())
}

fn registrations() -> Vec<Registration> {
    vec![Registration {
        name: "module_a",
        order: 10,
        register: module_a,
    }]
}

struct Fixture {
    plane: ControlPlane,
    signer: SoftwareSigner,
    audit: Arc<MemoryAuditSink>,
    ca_key_pem: String,
}

fn fixture() -> Fixture {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let ca_key = KeyPair::generate().unwrap();
    let ca_key_pem = ca_key.serialize_pem();
    let audit = Arc::new(MemoryAuditSink::new());
    let audit_sink: Arc<dyn AuditSink> = audit.clone();
    let plane = ControlPlane::build(BuildDeps {
        config: Arc::new(config),
        pool: None,
        clock: clock(),
        audit: audit_sink,
        nonces: Arc::new(MemoryNonceStore::new()),
        vault: Arc::new(MemVault(ca_key_pem.clone())),
        registrations: registrations(),
    })
    .unwrap();
    Fixture {
        plane,
        signer,
        audit,
        ca_key_pem,
    }
}

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    body: &str,
    header: Option<String>,
    peer: Option<PeerCertificate>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(header) = header {
        builder = builder.header("x-spot-command", header);
    }
    if let Some(peer) = peer {
        builder = builder.extension(peer);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn alice() -> PeerCertificate {
    PeerCertificate::new("CN=alice", "fp-alice")
}

fn envelope_header(f: &Fixture, path: &str, body: &str, audience: &str) -> String {
    let command = Command::new(
        "cp.cp_tenant_create",
        json!({ "body_sha256": sha256_hex(body.as_bytes()), "path": path }),
        audience,
        f.signer.key_id(),
        NOW - 5,
        NOW + 300,
    );
    let signed = sign(&command, &f.signer).unwrap();
    let text = spot_mgmt_api::wire::encode(&signed, &[]).unwrap();
    URL_SAFE_NO_PAD.encode(text.as_bytes())
}

#[tokio::test]
async fn it_starts_with_a_test_config_and_answers_liveness_and_readiness() {
    let f = fixture();
    assert_eq!(f.plane.module_names, vec!["module_a"]);
    let (status, body) = call(
        &f.plane.operator_router,
        "GET",
        "/cp/v1/live",
        "",
        None,
        None,
    )
    .await;
    assert_eq!(
        (status, &body["data"]["status"]),
        (StatusCode::OK, &json!("live"))
    );
    let (status, body) = call(
        &f.plane.operator_router,
        "GET",
        "/cp/v1/readiness",
        "",
        None,
        Some(alice()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["modules"]["module_a"], true);
}

#[tokio::test]
async fn an_operator_outside_the_scope_of_a_tenant_gets_not_found() {
    let f = fixture();
    let (status, body) = call(
        &f.plane.operator_router,
        "GET",
        "/cp/v1/tenants/t1",
        "",
        None,
        Some(alice()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &f.plane.operator_router,
        "GET",
        "/cp/v1/tenants/t2",
        "",
        None,
        Some(alice()),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("cp.not_found"))
    );
    let root = PeerCertificate::new("CN=root", "fp-root");
    let (status, _) = call(
        &f.plane.operator_router,
        "GET",
        "/cp/v1/tenants/t2",
        "",
        None,
        Some(root),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unsigned_mutation_is_refused_and_a_bound_signed_one_succeeds() {
    let f = fixture();
    let router = &f.plane.operator_router;
    let body = r#"{"name":"acme"}"#;
    let (status, answer) = call(router, "POST", "/cp/v1/tenants", body, None, Some(alice())).await;
    assert_eq!(
        (status, answer["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("cp.signature_invalid"))
    );
    // Signed, but for another body.
    let header = envelope_header(&f, "/cp/v1/tenants", "other", "cp-eu-1");
    let (status, _) = call(
        router,
        "POST",
        "/cp/v1/tenants",
        body,
        Some(header),
        Some(alice()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Signed for another audience.
    let header = envelope_header(&f, "/cp/v1/tenants", body, "cp-us-1");
    let (status, _) = call(
        router,
        "POST",
        "/cp/v1/tenants",
        body,
        Some(header),
        Some(alice()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Properly bound.
    let header = envelope_header(&f, "/cp/v1/tenants", body, "cp-eu-1");
    let (status, answer) = call(
        router,
        "POST",
        "/cp/v1/tenants",
        body,
        Some(header.clone()),
        Some(alice()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(answer["data"]["command"].is_string());
    // The same envelope again is a replay.
    let (status, answer) = call(
        router,
        "POST",
        "/cp/v1/tenants",
        body,
        Some(header),
        Some(alice()),
    )
    .await;
    assert_eq!(
        (status, answer["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("cp.replayed"))
    );
    let refused = f
        .audit
        .records()
        .iter()
        .filter(|r| r.outcome == spot_mgmt_api::ports::AuditOutcome::Refused)
        .count();
    assert_eq!(refused, 4);
}

#[tokio::test]
async fn certificates_do_not_cross_between_operator_and_host_routes() {
    let f = fixture();
    // An operator on the host router is not a host.
    let (status, body) = call(
        &f.plane.host_router,
        "GET",
        "/cp/v1/host/whoami",
        "",
        None,
        Some(alice()),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("cp.unauthenticated"))
    );
    // A host on the operator router is not an operator.
    let host = PeerCertificate::new("CN=spot-host/t1/h1", "fp-host");
    let (status, body) = call(
        &f.plane.operator_router,
        "GET",
        "/cp/v1/tenants/t1",
        "",
        None,
        Some(host.clone()),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("cp.forbidden"))
    );
    // A host gets its own tenant, derived from the certificate.
    let (status, body) = call(
        &f.plane.host_router,
        "GET",
        "/cp/v1/host/whoami",
        "",
        None,
        Some(host),
    )
    .await;
    assert_eq!(
        (status, &body["data"]["tenant"]),
        (StatusCode::OK, &json!("t1"))
    );
}

#[tokio::test]
async fn the_host_ca_issues_short_lived_certificates_and_revocation_is_enforced() {
    let f = fixture();
    let issued = f.plane.host_ca.issue("t1", "h1").await.unwrap();
    assert_eq!(issued.not_after, NOW + 30 * 86_400);
    let der = rustls_pemfile_free_der(&issued.certificate_pem);
    let peer = PeerCertificate::from_der(&der);
    assert_eq!(peer.subject(), Some("CN=spot-host/t1/h1"));
    assert_eq!(peer.fingerprint(), issued.fingerprint);
    let (status, _) = call(
        &f.plane.host_router,
        "GET",
        "/cp/v1/host/whoami",
        "",
        None,
        Some(peer.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Renewal over the existing certificate keeps the identity.
    let renewed = f
        .plane
        .host_ca
        .renew(&HostIdentity::from_peer(&peer).unwrap())
        .await
        .unwrap();
    assert_eq!(
        (
            renewed.identity.tenant.as_str(),
            renewed.identity.host_id.as_str()
        ),
        ("t1", "h1")
    );
    f.plane
        .host_ca
        .revoke(peer.fingerprint(), "root")
        .await
        .unwrap();
    let (status, body) = call(
        &f.plane.host_router,
        "GET",
        "/cp/v1/host/whoami",
        "",
        None,
        Some(peer),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("cp.unauthenticated"))
    );
    // Issuance and revocation are audited.
    let actions: Vec<String> = f.audit.records().iter().map(|r| r.action.clone()).collect();
    assert!(actions.contains(&"cp.host_ca.issue".to_owned()));
    assert!(actions.contains(&"cp.host_ca.revoke".to_owned()));
    // A bad identity is refused.
    assert!(f.plane.host_ca.issue("T1/..", "h1").await.is_err());
}

/// The first certificate of a PEM text as DER (rcgen's `pem` is plain base64 between markers).
fn rustls_pemfile_free_der(pem: &str) -> Vec<u8> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .unwrap()
}

#[test]
fn the_ca_refuses_a_key_file_that_others_can_read() {
    use spot_control_plane::auth::FileKeyVault;
    let dir = std::env::temp_dir().join(format!("spot-cp-vault-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ca.key");
    std::fs::write(&path, KeyPair::generate().unwrap().serialize_pem()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(FileKeyVault::new(path.clone()).ca_key_pem().is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(FileKeyVault::new(path.clone()).ca_key_pem().is_ok());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_module_registry_orders_and_detects_duplicates() {
    fn noop(_: &mut CpCtx) -> Result<(), CpError> {
        Ok(())
    }
    let reg = |name, order| Registration {
        name,
        order,
        register: noop,
    };
    let ok = ModuleRegistry::new(&[reg("b", 20), reg("a", 10), reg("c", 30)]).unwrap();
    assert_eq!(ok.names(), vec!["a", "b", "c"]);
    assert!(matches!(
        ModuleRegistry::new(&[reg("a", 10), reg("a", 20)]),
        Err(RegistryError::DuplicateName("a"))
    ));
    assert!(matches!(
        ModuleRegistry::new(&[reg("a", 10), reg("b", 10)]),
        Err(RegistryError::DuplicateOrder(..))
    ));
}

#[test]
fn unknown_keys_and_unsafe_combinations_refuse_the_start() {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let good = config_text(&hex(&signer.public_key_bytes()));
    assert!(Config::from_toml(&good).is_ok());
    let extra = format!("{good}\nsurprise = 1\n");
    assert!(matches!(
        Config::from_toml(&extra),
        Err(ConfigError::Parse(_))
    ));
    let same_ca = good.replace(
        "/nonexistent/host-ca.pem\"\n\n[host_ca]",
        "/nonexistent/operator-ca.pem\"\n\n[host_ca]",
    );
    assert!(matches!(
        Config::from_toml(&same_ca),
        Err(ConfigError::Invalid(_))
    ));
    let bad_key = good.replace(&hex(&signer.public_key_bytes()), "zz");
    assert!(matches!(
        Config::from_toml(&bad_key),
        Err(ConfigError::Invalid(_))
    ));
}

#[tokio::test]
async fn check_reports_each_failure_separately() {
    let signer = SoftwareSigner::from_seed(&[9u8; 32]).unwrap();
    let config = Config::from_toml(&config_text(&hex(&signer.public_key_bytes()))).unwrap();
    let items = check(&config, clock().as_ref()).await;
    let by_name = |name: &str| items.iter().find(|i| i.name == name).unwrap().clone();
    for failing in [
        "operator_listener_tls",
        "host_listener_tls",
        "host_ca_key",
        "database",
    ] {
        assert!(!by_name(failing).ok, "{failing} should fail");
        assert!(!by_name(failing).detail.is_empty());
    }
    assert!(by_name("clock").ok);
}

#[test]
fn no_module_depends_on_the_tenant_data_crates() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success(), "cargo metadata failed");
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let nodes = metadata["resolve"]["nodes"].as_array().unwrap();
    let id_of = |name: &str| -> Option<String> {
        metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .map(|p| p["id"].as_str().unwrap().to_owned())
    };
    let root = id_of("spot-control-plane").unwrap();
    // Walk the whole dependency closure of the control plane.
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![root.clone()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(node) = nodes.iter().find(|n| n["id"] == id.as_str()) {
            for dep in node["deps"].as_array().unwrap() {
                stack.push(dep["pkg"].as_str().unwrap().to_owned());
            }
        }
    }
    let names: std::collections::BTreeSet<String> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| seen.contains(p["id"].as_str().unwrap()))
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    for forbidden in [
        "spot-document",
        "spot-doc-tables",
        "spot-files",
        "spot-journal-store",
        "spot-views",
        "spot-reports",
    ] {
        assert!(
            !names.contains(forbidden),
            "the control plane depends on {forbidden}"
        );
    }
    // The storage contract may only be the database driver's own dependency.
    let root_node = nodes.iter().find(|n| n["id"] == root.as_str()).unwrap();
    let direct: Vec<&str> = root_node["deps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(!direct.contains(&"spot_storage_contract"));
}

#[test]
fn control_plane_codes_match_the_contract_registry() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/control-plane/openapi.yaml"
    );
    let text = std::fs::read_to_string(path).unwrap();
    let mut documented = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        let Some((name, rest)) = trimmed.split_once(": [") else {
            continue;
        };
        if !name.starts_with("cp.") {
            continue;
        }
        documented += 1;
        let mut parts = rest.split(',');
        let status: u16 = parts.next().unwrap().trim().parse().unwrap();
        let retryable = parts.next().unwrap().trim() == "true";
        let code = CpCode::ALL.iter().find(|c| c.as_str() == name).unwrap();
        assert_eq!(
            (code.status().as_u16(), code.retryable()),
            (status, retryable),
            "{name}"
        );
    }
    assert_eq!(documented, CpCode::ALL.len());
}

// ---- Real sockets: separate CAs, handshake refusal ---------------------------------------

mod sockets {
    use super::*;
    use rcgen::{ExtendedKeyUsagePurpose, Issuer};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use spot_mgmt_api::tls::{HandshakeLimiter, serve, tls_config};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    struct Ca {
        params: CertificateParams,
        key: KeyPair,
        der: CertificateDer<'static>,
    }

    impl Ca {
        fn new(name: &str, key: KeyPair) -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.distinguished_name.push(DnType::CommonName, name);
            let der = params.self_signed(&key).unwrap().der().clone();
            Self { params, key, der }
        }

        fn issue(
            &self,
            cn: &str,
            sans: Vec<String>,
            client: bool,
        ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
            let issuer = Issuer::from_params(&self.params, &self.key);
            let mut params = CertificateParams::new(sans).unwrap();
            params.distinguished_name.push(DnType::CommonName, cn);
            params.extended_key_usages = vec![if client {
                ExtendedKeyUsagePurpose::ClientAuth
            } else {
                ExtendedKeyUsagePurpose::ServerAuth
            }];
            let key = KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &issuer).unwrap();
            (
                cert.der().clone(),
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
        }
    }

    async fn start(router: Router, server_ca: &Ca, client_ca: &Ca) -> String {
        let (chain, key) = server_ca.issue("localhost", vec!["127.0.0.1".to_owned()], false);
        let config = tls_config(vec![chain], key, std::slice::from_ref(&client_ca.der)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ = serve(
                listener,
                Arc::new(config),
                router,
                clock(),
                Arc::new(HandshakeLimiter::standard()),
                std::future::pending::<()>(),
            )
            .await;
        });
        address
    }

    async fn get(
        address: &str,
        server_ca: &Ca,
        client: (CertificateDer<'static>, PrivateKeyDer<'static>),
        path: &str,
    ) -> Option<String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(server_ca.der.clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![client.0], client.1)
            .unwrap();
        let tcp = TcpStream::connect(address).await.ok()?;
        let name = ServerName::try_from("127.0.0.1".to_owned()).unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .ok()?;
        tls.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .ok()?;
        let mut buffer = Vec::new();
        tls.read_to_end(&mut buffer).await.ok()?;
        let text = String::from_utf8_lossy(&buffer).into_owned();
        text.starts_with("HTTP/1.1").then_some(text)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_ca_client_is_rejected_on_each_listener() {
        let f = fixture();
        let operator_ca = Ca::new("operator CA", KeyPair::generate().unwrap());
        // The host CA of the service: same key and name the `HostCa` uses.
        let host_ca = Ca::new("Spot host CA", KeyPair::from_pem(&f.ca_key_pem).unwrap());
        let server_ca = Ca::new("server CA", KeyPair::generate().unwrap());

        let op_address = start(f.plane.operator_router.clone(), &server_ca, &operator_ca).await;
        let host_address = start(f.plane.host_router.clone(), &server_ca, &host_ca).await;

        // Operator listener: an operator certificate works; a host-CA certificate is refused.
        let operator = operator_ca.issue("alice", Vec::new(), true);
        let ok = get(&op_address, &server_ca, operator, "/cp/v1/tenants/t1")
            .await
            .unwrap();
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let host_cert = host_ca.issue("spot-host/t1/h1", Vec::new(), true);
        assert!(
            get(&op_address, &server_ca, host_cert, "/cp/v1/tenants/t1")
                .await
                .is_none()
        );

        // Host listener: a host certificate works; an operator-CA certificate is refused.
        let host_cert = host_ca.issue("spot-host/t1/h1", Vec::new(), true);
        let ok = get(&host_address, &server_ca, host_cert, "/cp/v1/host/whoami")
            .await
            .unwrap();
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let operator = operator_ca.issue("alice", Vec::new(), true);
        assert!(
            get(&host_address, &server_ca, operator, "/cp/v1/host/whoami")
                .await
                .is_none()
        );
    }
}

// ---- PostgreSQL ------------------------------------------------------------------------

#[tokio::test]
async fn migrate_is_idempotent() {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let mut config = deadpool_postgres::Config::new();
    config.url = Some(url);
    let pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();
    let migrations = [spot_control_plane::CORE_MIGRATION];
    spot_control_plane::migrate(&pool, &migrations)
        .await
        .unwrap();
    spot_control_plane::migrate(&pool, &migrations)
        .await
        .unwrap();
}
