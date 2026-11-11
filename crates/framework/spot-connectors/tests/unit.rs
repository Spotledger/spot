//! Unit-level tests for the vault (OAuth2), the mappings and the monitor.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use spot_connectors::{
    Coercion, ConnectorError, ConnectorId, ConnectorMonitor, CredentialRef, CredentialVault,
    InMemoryKv, KvStore, MappingRule, MappingSpec, MonitorConfig, OAuth2Config, Refusal,
    TokenTransport, Vault, check_mapping, testkit::MockTokenTransport,
};
use spot_secrets::SecretString;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_refreshes_once_under_concurrency() {
    let transport = Arc::new(MockTokenTransport::new(
        r#"{"access_token":"tok-1","expires_in":3600}"#,
    ));
    let mut vault = Vault::new().with_transport(transport.clone());
    vault.insert_static(
        CredentialRef::new("client-secret").unwrap(),
        SecretString::new("cs"),
    );
    vault.insert_oauth2(
        CredentialRef::new("api").unwrap(),
        OAuth2Config {
            token_url: "https://auth.example.test/token".to_owned(),
            client_id: "client".to_owned(),
            client_secret: CredentialRef::new("client-secret").unwrap(),
            scopes: vec!["read".to_owned()],
        },
    );
    let vault = Arc::new(vault);

    let mut handles = Vec::new();
    for _ in 0..16 {
        let vault = Arc::clone(&vault);
        handles.push(tokio::spawn(async move {
            vault
                .resolve(&CredentialRef::new("api").unwrap(), 0)
                .await
                .unwrap()
                .expose()
                .to_owned()
        }));
    }
    for handle in handles {
        assert_eq!(handle.await.unwrap(), "tok-1");
    }
    assert_eq!(
        transport.calls(),
        1,
        "the token was refreshed more than once"
    );
    assert_eq!(vault.refresh_count(), 1);
}

#[tokio::test]
async fn an_expired_oauth_token_is_refreshed() {
    let transport = Arc::new(MockTokenTransport::new(
        r#"{"access_token":"tok-2","expires_in":10}"#,
    ));
    let mut vault = Vault::new().with_transport(transport.clone());
    vault.insert_static(
        CredentialRef::new("client-secret").unwrap(),
        SecretString::new("cs"),
    );
    vault.insert_oauth2(
        CredentialRef::new("api").unwrap(),
        OAuth2Config {
            token_url: "https://auth.example.test/token".to_owned(),
            client_id: "client".to_owned(),
            client_secret: CredentialRef::new("client-secret").unwrap(),
            scopes: Vec::new(),
        },
    );
    // The first resolve refreshes; a second within the window reuses the cache.
    vault
        .resolve(&CredentialRef::new("api").unwrap(), 0)
        .await
        .unwrap();
    vault
        .resolve(&CredentialRef::new("api").unwrap(), 1_000_000)
        .await
        .unwrap();
    assert_eq!(transport.calls(), 1);
    // Past the (skewed) expiry, it refreshes again.
    vault
        .resolve(&CredentialRef::new("api").unwrap(), 20_000_000)
        .await
        .unwrap();
    assert_eq!(transport.calls(), 2);
}

#[test]
fn mappings_apply_and_report() {
    let spec = MappingSpec {
        rules: vec![
            MappingRule {
                target: "out.ref".to_owned(),
                source: "payload.id".to_owned(),
                expression: String::new(),
                coerce: Coercion::Text,
            },
            MappingRule {
                target: "out.total".to_owned(),
                source: "payload.price".to_owned(),
                expression: String::new(),
                coerce: Coercion::Decimal,
            },
            MappingRule {
                target: "out.qty".to_owned(),
                source: String::new(),
                expression: "doc.payload.qty + 1".to_owned(),
                coerce: Coercion::Int,
            },
        ],
    };
    let sample = json!({"payload": {"id": 7, "price": "2.50", "qty": 3}});
    let report = check_mapping(&spec, &sample, 0);
    assert!(report.all_ok(), "{report:?}");

    let out = spec.apply(&sample, 0).unwrap();
    assert_eq!(out["out"]["ref"], json!("7"));
    assert_eq!(out["out"]["total"], json!("2.50"));
    assert_eq!(out["out"]["qty"], json!(4));
}

#[test]
fn a_bad_mapping_rule_is_reported_not_panicked() {
    let spec = MappingSpec {
        rules: vec![MappingRule {
            target: "out.value".to_owned(),
            source: String::new(),
            expression: "doc.missing + 1".to_owned(),
            coerce: Coercion::Int,
        }],
    };
    let report = check_mapping(&spec, &json!({}), 0);
    assert!(!report.all_ok());
    assert!(report.outcomes[0].error.is_some());
    let err = spec.apply(&json!({}), 0).unwrap_err();
    assert_eq!(err.code(), "connectors.mapping");
}

#[test]
fn the_breaker_opens_then_half_opens() {
    let monitor = ConnectorMonitor::new(MonitorConfig {
        failure_threshold: 2,
        open_secs: 30,
        half_open_probes: 1,
    });
    let connector = ConnectorId::new("c").unwrap();
    let timeout = ConnectorError::Timeout { millis: 1 };

    for _ in 0..2 {
        let permit = monitor.acquire(&connector, "t", 0).unwrap();
        permit.record_failure(&timeout, "invoke", 0);
    }
    assert!(monitor.is_open(&connector, "t", 0));
    assert!(matches!(
        monitor.acquire(&connector, "t", 0),
        Err(Refusal::CircuitOpen { .. })
    ));

    // After the window, one probe passes and closes the breaker.
    let permit = monitor.acquire(&connector, "t", 31_000_000).unwrap();
    permit.record_success("invoke");
    assert!(!monitor.is_open(&connector, "t", 31_000_000));
    assert_eq!(monitor.stats(&connector, "t").calls, 3);
}

#[tokio::test]
async fn connector_metrics_are_exported() {
    let _ = spot_telemetry::metrics::install();
    let monitor = Arc::new(ConnectorMonitor::new(MonitorConfig::default()));
    let connector = ConnectorId::new("metric").unwrap();
    let permit = monitor.acquire(&connector, "t", 0).unwrap();
    permit.record_success("invoke");
    monitor.record_latency(&connector, "invoke", 1_500);
    let text = spot_telemetry::render_prometheus();
    assert!(
        text.contains("spot_connector_calls_total"),
        "the connector counter is not exported"
    );
    assert!(
        text.contains("spot_connector_call_duration_seconds"),
        "the connector histogram is not exported"
    );
}

#[test]
fn the_sdk_template_ships_the_same_world() {
    // The SDK template must not drift from the host's world.
    let host = include_str!("../wit/connector.wit");
    let template = include_str!("../../../../sdk/connector-template/wit/connector.wit");
    let sample =
        include_str!("../../../../sdk/connector-template/examples/echo-http/wit/connector.wit");
    assert_eq!(host, template);
    assert_eq!(host, sample);
}

#[tokio::test]
async fn a_sealed_credential_is_opened_at_call_time() {
    let keyring = spot_secrets::MemoryKeyring::new("connectors").unwrap();
    let mut vault = spot_connectors::SealedVault::new(keyring);
    vault
        .insert(
            CredentialRef::new("api-token").unwrap(),
            &SecretString::new("sealed-token"),
            b"connector=echo",
        )
        .await
        .unwrap();
    let resolved = vault
        .resolve(&CredentialRef::new("api-token").unwrap(), 0)
        .await
        .unwrap();
    assert_eq!(resolved.expose(), "sealed-token");
    // A reference the vault does not hold is unavailable, never a value.
    let err = vault
        .resolve(&CredentialRef::new("other").unwrap(), 0)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "connectors.secret_unavailable");
}

/// A transport that answers a different token every time.
struct CountingTransport(std::sync::atomic::AtomicU64);

#[async_trait::async_trait]
impl TokenTransport for CountingTransport {
    async fn post_form(
        &self,
        _url: &str,
        _form: &str,
        _now: i64,
    ) -> spot_connectors::Result<String> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Ok(format!(r#"{{"access_token":"tok-{n}","expires_in":3600}}"#))
    }
}

#[tokio::test]
async fn an_oauth_token_is_never_served_to_another_tenant() {
    let mut vault = Vault::new().with_transport(Arc::new(CountingTransport(
        std::sync::atomic::AtomicU64::new(0),
    )));
    vault.insert_static(
        CredentialRef::new("client-secret").unwrap(),
        SecretString::new("cs"),
    );
    vault.insert_oauth2(
        CredentialRef::new("api").unwrap(),
        OAuth2Config {
            token_url: "https://auth.example.test/token".to_owned(),
            client_id: "client".to_owned(),
            client_secret: CredentialRef::new("client-secret").unwrap(),
            scopes: vec![],
        },
    );
    let api = CredentialRef::new("api").unwrap();
    let a = vault.resolve_for("tenant-a", &api, 0).await.unwrap();
    let b = vault.resolve_for("tenant-b", &api, 1).await.unwrap();
    assert_ne!(a.expose(), b.expose(), "tenant B got tenant A's token");
    // Each tenant keeps hitting its own cached token.
    let a2 = vault.resolve_for("tenant-a", &api, 2).await.unwrap();
    let b2 = vault.resolve_for("tenant-b", &api, 3).await.unwrap();
    assert_eq!(a.expose(), a2.expose());
    assert_eq!(b.expose(), b2.expose());
    assert_eq!(vault.refresh_count(), 2);
    // The tenant-less bucket is a third, separate one.
    let none = vault.resolve(&api, 4).await.unwrap();
    assert_ne!(none.expose(), a.expose());
    assert_ne!(none.expose(), b.expose());
}

#[test]
fn the_kv_store_is_per_tenant() {
    let kv = InMemoryKv::new();
    let connector = ConnectorId::new("c").unwrap();
    kv.set("tenant-a", &connector, "cursor", "a-value", 1024)
        .unwrap();
    assert_eq!(
        kv.get("tenant-a", &connector, "cursor").as_deref(),
        Some("a-value")
    );
    assert_eq!(kv.get("tenant-b", &connector, "cursor"), None);
    // B writing the same key does not overwrite A's value.
    kv.set("tenant-b", &connector, "cursor", "b-value", 1024)
        .unwrap();
    assert_eq!(
        kv.get("tenant-a", &connector, "cursor").as_deref(),
        Some("a-value")
    );
    // The budget is per tenant: A filling its budget leaves B's untouched.
    assert!(
        kv.set("tenant-a", &connector, "big", &"x".repeat(2000), 1024)
            .is_err()
    );
    kv.set("tenant-b", &connector, "k", &"x".repeat(900), 1024)
        .unwrap();
}
