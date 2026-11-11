//! The host end to end: connector components built from text are compiled, linked and called,
//! with egress, secrets, fuel and the breaker exercised.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::json;
use spot_connectors::{
    AuthKind, AuthSpec, ConnectorCall, ConnectorConfig, ConnectorHost, ConnectorId,
    ConnectorMonitor, ConnectorPort, ConnectorRuntime, CredentialRef, HttpRequest, MonitorConfig,
    Vault, testkit::MockHttp,
};
use spot_core::ManualClock;
use spot_egress::{EgressError, EgressPolicy, Guard, Sender, TenantPolicy};
use spot_wasm::{HostConfig, Limits};
use time::OffsetDateTime;

fn runtime() -> Arc<ConnectorRuntime> {
    Arc::new(ConnectorRuntime::new(HostConfig::default()).expect("the runtime builds"))
}

fn monitor() -> Arc<ConnectorMonitor> {
    Arc::new(ConnectorMonitor::new(MonitorConfig::default()))
}

fn new_host(http: Arc<MockHttp>, vault: Arc<Vault>) -> ConnectorHost {
    ConnectorHost::new(runtime(), vault, http, monitor())
}

fn id(name: &str) -> ConnectorId {
    ConnectorId::new(name).expect("a non-empty id")
}

fn call(connector: &str, operation: &str, input: serde_json::Value, tenant: &str) -> ConnectorCall {
    ConnectorCall {
        connector: id(connector),
        operation: operation.to_owned(),
        input,
        tenant: tenant.to_owned(),
        actor: None,
        now_micros: 0,
        deadline_micros: 1_000_000,
    }
}

#[tokio::test]
async fn a_sample_connector_is_described_and_invoked() {
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(Vault::new()));
    let info = host
        .install(ConnectorConfig::new(id("echo")), &component(&ECHO))
        .await
        .expect("the connector installs");
    assert_eq!(info.name, "connector");
    assert_eq!(info.version, "0.1.0");
    assert_eq!(info.auth, vec![AuthKind::None]);

    let out = host
        .invoke(call("echo", "echo", json!({"a": 1}), "t1"))
        .await
        .expect("the connector answers");
    assert_eq!(out.value, json!({"a": 1}));

    // `describe` through the port.
    let port: &dyn ConnectorPort = &host;
    assert_eq!(port.describe(&id("echo")).await.unwrap().name, "connector");
    assert!(port.ping(&id("echo")).await.unwrap().healthy);
}

#[tokio::test]
async fn a_webhook_returns_the_connectors_response() {
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(Vault::new()));
    host.install(ConnectorConfig::new(id("hook")), &component(&WEBHOOK))
        .await
        .expect("the connector installs");
    let response = host
        .deliver_webhook(
            &id("hook"),
            HttpRequest {
                method: "POST".into(),
                url: "https://example.test/hook".into(),
                headers: Vec::new(),
                body: Some("{}".into()),
            },
            "t1",
            0,
        )
        .await
        .expect("the webhook is delivered");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "ok");
}

#[tokio::test]
async fn an_outbound_request_reaches_the_recorded_host() {
    let http = Arc::new(MockHttp::new());
    http.record_url("https://api.example.test", 200, r#"{"ok":true}"#);
    let host = new_host(Arc::clone(&http), Arc::new(Vault::new()));
    host.install(ConnectorConfig::new(id("http")), &component(&HTTP))
        .await
        .expect("the connector installs");

    let out = host
        .invoke(call("http", "ping", json!({}), "t1"))
        .await
        .expect("the request is made");
    assert_eq!(out.value["status"], json!(200));
    assert_eq!(out.value["body"], json!(r#"{"ok":true}"#));

    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://api.example.test/ping");
    assert_eq!(requests[0].method, "GET");
}

/// A sender that is never reached: the guard denies before any connection.
struct NeverSender;

#[async_trait::async_trait]
impl Sender for NeverSender {
    async fn send_request(
        &self,
        _request: &spot_egress::HttpRequest,
        _authorized: &spot_egress::Authorized,
        _max_body: u64,
    ) -> Result<spot_egress::Response, EgressError> {
        panic!("a denied request must never reach the sender")
    }
}

#[tokio::test]
async fn a_disallowed_host_is_denied_by_the_egress_policy() {
    // A tenant with an empty allow-list and the default (deny) policy: every host is refused.
    let mut policy = EgressPolicy::default();
    policy
        .tenants
        .insert("t1".to_owned(), TenantPolicy::default());
    let guard = Guard::with_defaults(
        policy,
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    );
    let http = Arc::new(spot_connectors::EgressHttp::new(
        Arc::new(guard),
        Arc::new(NeverSender),
        1024 * 1024,
    ));

    // The transport itself maps the policy denial to `connector.egress_denied`, before any
    // connection.
    let direct = spot_connectors::HttpService::send(
        http.as_ref(),
        HttpRequest {
            method: "GET".into(),
            url: "https://api.example.test/ping".into(),
            headers: Vec::new(),
            body: None,
        },
        &spot_connectors::HttpContext {
            tenant: "t1".into(),
            actor: None,
            now_micros: 0,
        },
    )
    .await
    .expect_err("the destination is denied");
    assert_eq!(direct.code(), "connectors.egress_denied");
    assert!(!direct.is_retryable());

    let host = ConnectorHost::new(runtime(), Arc::new(Vault::new()), http, monitor());
    host.install(ConnectorConfig::new(id("http")), &component(&HTTP))
        .await
        .expect("the connector installs");

    // Through the guest, the denial is an error the connector returns itself.
    let err = host
        .invoke(call("http", "ping", json!({}), "t1"))
        .await
        .expect_err("the destination is denied");
    assert_eq!(err.code(), "connectors.guest");
}

#[tokio::test]
async fn a_credential_is_injected_and_never_reaches_the_guest() {
    const TOKEN: &str = "s3cr3t-canary-token";
    let http = Arc::new(MockHttp::new());
    http.record_url("https://api.example.test", 200, "done");

    let mut vault = Vault::new();
    vault.insert_static(
        CredentialRef::new("api-token").unwrap(),
        spot_secrets::SecretString::new(TOKEN),
    );
    let logs = Arc::new(spot_connectors::RecordingLogSink::default());
    let host = ConnectorHost::new(runtime(), Arc::new(vault), http.clone(), monitor())
        .with_logs(logs.clone());

    let mut config = ConnectorConfig::new(id("http"));
    config.endpoints = vec!["api.example.test".to_owned()];
    config.auth = Some(AuthSpec {
        kind: AuthKind::Bearer,
        credential: CredentialRef::new("api-token").unwrap(),
        username: None,
        header: None,
    });
    host.install(config, &component(&HTTP))
        .await
        .expect("the connector installs");

    let out = host
        .invoke(call("http", "ping", json!({}), "t1"))
        .await
        .expect("the request is made");

    // The host injected the token into the outbound request...
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    let auth = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone());
    assert_eq!(auth.as_deref(), Some("Bearer s3cr3t-canary-token"));

    // ...and the guest never saw it: not in its logs, not in what it returned.
    assert!(
        logs.lines().iter().all(|(_, line)| !line.contains(TOKEN)),
        "a log line carried the token"
    );
    assert!(
        !out.value.to_string().contains(TOKEN),
        "the output carried the token"
    );
}

#[tokio::test]
async fn an_injected_credential_is_not_readable_and_a_declared_one_is() {
    let mut vault = Vault::new();
    vault.insert_static(
        CredentialRef::new("api-token").unwrap(),
        spot_secrets::SecretString::new("injected"),
    );
    vault.insert_static(
        CredentialRef::new("shared").unwrap(),
        spot_secrets::SecretString::new("visible"),
    );
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(vault));

    let mut config = ConnectorConfig::new(id("secret"));
    config.endpoints = vec!["api.example.test".to_owned()];
    config.auth = Some(AuthSpec {
        kind: AuthKind::Bearer,
        credential: CredentialRef::new("api-token").unwrap(),
        username: None,
        header: None,
    });
    config.secrets = vec![CredentialRef::new("shared").unwrap()];
    host.install(config, &component(&SECRET))
        .await
        .expect("the connector installs");

    // The injected credential is refused: the guest gets the connector's own error.
    let err = host
        .invoke(call("secret", "read", json!({}), "t1"))
        .await
        .expect_err("the injected credential is refused");
    assert_eq!(err.code(), "connectors.guest");

    // A declared, guest-visible credential is readable.
    let mut vault2 = Vault::new();
    vault2.insert_static(
        CredentialRef::new("shared").unwrap(),
        spot_secrets::SecretString::new("\"visible\""),
    );
    let host2 = new_host(Arc::new(MockHttp::new()), Arc::new(vault2));
    let mut config2 = ConnectorConfig::new(id("secret2"));
    config2.secrets = vec![CredentialRef::new("shared").unwrap()];
    host2
        .install(config2, &component(&SECRET.with_secret_ref("shared")))
        .await
        .expect("the connector installs");
    let out = host2
        .invoke(call("secret2", "read", json!({}), "t1"))
        .await
        .expect("the declared credential is read");
    assert_eq!(out.value, json!("visible"));
}

#[tokio::test]
async fn fuel_exhaustion_kills_only_the_call() {
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(Vault::new()));
    let mut spin = ConnectorConfig::new(id("spin"));
    spin.limits = Limits {
        fuel: 1_000_000,
        wall_clock: Duration::from_secs(20),
        ..Limits::default()
    };
    host.install(spin, &component(&SPIN))
        .await
        .expect("the connector installs");
    host.install(ConnectorConfig::new(id("echo")), &component(&ECHO))
        .await
        .expect("the connector installs");

    let err = host
        .invoke(call("spin", "loop", json!({}), "t1"))
        .await
        .expect_err("the loop runs out of fuel");
    assert_eq!(err.code(), "connectors.out_of_fuel");
    assert!(err.is_retryable());

    // The host is fine: another connector answers.
    let out = host
        .invoke(call("echo", "echo", json!({"still": "alive"}), "t1"))
        .await
        .expect("the host survives");
    assert_eq!(out.value, json!({"still": "alive"}));
}

#[tokio::test]
async fn the_breaker_opens_after_failures() {
    let monitor = Arc::new(ConnectorMonitor::new(MonitorConfig {
        failure_threshold: 2,
        open_secs: 30,
        half_open_probes: 1,
    }));
    let host = ConnectorHost::new(
        runtime(),
        Arc::new(Vault::new()),
        Arc::new(MockHttp::new()),
        Arc::clone(&monitor),
    );
    host.install(ConnectorConfig::new(id("fails")), &component(&FAILS))
        .await
        .expect("the connector installs");

    for _ in 0..2 {
        let err = host
            .invoke(call("fails", "op", json!({}), "t1"))
            .await
            .expect_err("the connector fails");
        assert_eq!(err.code(), "connectors.guest");
    }
    // The third call is refused without reaching the connector.
    let err = host
        .invoke(call("fails", "op", json!({}), "t1"))
        .await
        .expect_err("the breaker is open");
    assert_eq!(err.code(), "connectors.circuit_open");
    assert_eq!(monitor.stats(&id("fails"), "t1").failures, 2);

    // Another tenant has its own breaker.
    let err = host
        .invoke(call("fails", "op", json!({}), "t2"))
        .await
        .expect_err("the other tenant is admitted");
    assert_eq!(err.code(), "connectors.guest");
}

#[tokio::test]
async fn an_unknown_connector_is_not_found() {
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(Vault::new()));
    let err = host
        .invoke(call("missing", "op", json!({}), "t1"))
        .await
        .expect_err("the connector is not installed");
    assert_eq!(err.code(), "connectors.not_found");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn a_bundle_that_is_not_a_bundle_is_refused() {
    use spot_bundle_signing::{
        BundleSignature, CertificateChain, PublisherCertificate, RevocationBody, RevocationList,
        TrustAnchor, TrustedRoot, VerifyMode, VerifyPolicy, generate_keypair, issue_certificate,
    };

    let root = generate_keypair(&[7u8; 32]);
    let anchor = TrustAnchor::new(vec![TrustedRoot::new(root.public(), 0, None)]);
    let certificate = PublisherCertificate {
        publisher_id: "acme".to_owned(),
        display_name: "Acme".to_owned(),
        key: root.public(),
        valid_from: 0,
        valid_to: 1_000_000_000,
        issued_by: root.public().key_id(),
        serial: 1,
    };
    let chain = CertificateChain::new(issue_certificate(certificate, &root));
    let revocations = RevocationList::from_parts(
        RevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: root.public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        [0u8; 64],
    );
    let signature = BundleSignature::from_parts(
        "echo".to_owned(),
        "0.1.0".to_owned(),
        "acme".to_owned(),
        root.public().key_id(),
        0,
        [0u8; 64],
    );
    let policy = VerifyPolicy {
        mode: VerifyMode::Install,
        allow_expired_certificate_at_startup: false,
        held_revocation_generation: 0,
    };
    let signed = spot_connectors::SignedBundle {
        bytes: b"this is not a bundle",
        signature: &signature,
        chain: &chain,
        anchor: &anchor,
        revocations: &revocations,
        now: 0,
        policy: &policy,
    };

    let host = new_host(Arc::new(MockHttp::new()), Arc::new(Vault::new()));
    let err = host
        .install_bundle(ConnectorConfig::new(id("echo")), signed)
        .await
        .expect_err("the bytes are not a bundle");
    assert_eq!(err.code(), "connectors.bad_bundle");
}

#[tokio::test]
async fn a_credential_is_never_sent_to_an_undeclared_host() {
    let http = Arc::new(MockHttp::new());
    http.record_url("https://api.example.test", 200, "done");
    let mut vault = Vault::new();
    vault.insert_static(
        CredentialRef::new("api-token").unwrap(),
        spot_secrets::SecretString::new("s3cr3t-canary-token"),
    );
    let host = ConnectorHost::new(runtime(), Arc::new(vault), http.clone(), monitor());
    let mut config = ConnectorConfig::new(id("http"));
    config.endpoints = vec!["other.example.test".to_owned()];
    config.auth = Some(AuthSpec {
        kind: AuthKind::Bearer,
        credential: CredentialRef::new("api-token").unwrap(),
        username: None,
        header: None,
    });
    host.install(config, &component(&HTTP))
        .await
        .expect("the connector installs");
    let result = host.invoke(call("http", "ping", json!({}), "t1")).await;
    assert!(
        result.is_err(),
        "the request to an undeclared host must fail"
    );
    assert!(http.requests().is_empty(), "nothing may be sent");
}

#[tokio::test]
async fn authentication_without_endpoints_is_refused_at_load() {
    let mut vault = Vault::new();
    vault.insert_static(
        CredentialRef::new("api-token").unwrap(),
        spot_secrets::SecretString::new("s3cr3t-canary-token"),
    );
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(vault));
    let mut config = ConnectorConfig::new(id("http"));
    config.auth = Some(AuthSpec {
        kind: AuthKind::Bearer,
        credential: CredentialRef::new("api-token").unwrap(),
        username: None,
        header: None,
    });
    let err = host
        .install(config.clone(), &component(&HTTP))
        .await
        .expect_err("auth with no endpoints must not install");
    assert_eq!(err.code(), "connectors.endpoints_required");
    // A blank entry declares nothing either.
    config.endpoints = vec!["  ".to_owned()];
    let err = host
        .install(config, &component(&HTTP))
        .await
        .expect_err("a blank endpoint is no endpoint");
    assert_eq!(err.code(), "connectors.endpoints_required");
}

#[tokio::test]
async fn a_connector_without_authentication_needs_no_endpoints() {
    let host = new_host(Arc::new(MockHttp::new()), Arc::new(Vault::new()));
    host.install(ConnectorConfig::new(id("http")), &component(&HTTP))
        .await
        .expect("no auth, no endpoints required");
}
