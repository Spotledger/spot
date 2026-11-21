//! Shared fixtures: a harness around the router with in-memory fakes and a signer.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use spot_core::ManualClock;
use spot_mgmt_api::handlers::HandlerRegistry;
use spot_mgmt_api::lifecycle::MemoryCommandStore;
use spot_mgmt_api::ports::{
    ApprovalCheck, AuditError, AuditRecord, AuditSink, BoxFuture, MemoryAuditSink, NoApprovals,
    OperatorAllowList, OperatorEntry,
};
use spot_mgmt_api::stubs::{stub_catalogue, stub_handlers};
use spot_mgmt_api::tls::PeerCertificate;
use spot_mgmt_api::{Deps, router, wire};
use spot_signed_command::{
    Command, MemoryNonceStore, NonceFuture, NonceStore, Signer, SoftwareSigner, StoreError, sign,
};
use tower::ServiceExt as _;

/// The fixed "now" of the harness clock.
pub const NOW: i64 = 1_800_000_000;
/// The site id (the audience).
pub const SITE: &str = "site-1";

/// A nonce store that is down.
pub struct DownNonces;

impl NonceStore for DownNonces {
    fn check_and_insert<'a>(
        &'a self,
        _key: &'a spot_signed_command::KeyId,
        _nonce: [u8; 16],
        _expires_at: i64,
        _now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        Box::pin(std::future::ready(Err(StoreError::Unavailable)))
    }
}

/// An audit sink that always fails.
pub struct DownAudit;

impl AuditSink for DownAudit {
    fn record(&self, _record: AuditRecord) -> BoxFuture<'_, Result<String, AuditError>> {
        Box::pin(std::future::ready(Err(AuditError)))
    }
}

/// Options for [`Harness::build`].
pub struct Options {
    pub nonces: Arc<dyn NonceStore>,
    pub audit_down: bool,
    pub approvals: Arc<dyn ApprovalCheck>,
    pub handlers: Option<HandlerRegistry>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            nonces: Arc::new(MemoryNonceStore::new()),
            audit_down: false,
            approvals: Arc::new(NoApprovals),
            handlers: None,
        }
    }
}

/// The router plus its fakes.
pub struct Harness {
    pub router: Router,
    pub audit: Arc<MemoryAuditSink>,
    pub commands: Arc<MemoryCommandStore>,
    pub signer: SoftwareSigner,
    counter: std::sync::atomic::AtomicUsize,
}

pub fn alice() -> PeerCertificate {
    PeerCertificate::new("CN=alice", "fp-alice")
}

pub fn mallory() -> PeerCertificate {
    PeerCertificate::new("CN=mallory", "fp-mallory")
}

impl Harness {
    pub fn new() -> Self {
        Self::build(Options::default())
    }

    pub fn build(options: Options) -> Self {
        let signer = SoftwareSigner::from_seed(&[7u8; 32]).unwrap();
        let audit = Arc::new(MemoryAuditSink::new());
        let commands = Arc::new(MemoryCommandStore::new());
        let clock = Arc::new(ManualClock::new(
            time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        ));
        let audit_sink: Arc<dyn AuditSink> = if options.audit_down {
            Arc::new(DownAudit)
        } else {
            audit.clone()
        };
        let deps = Deps {
            catalogue: Arc::new(stub_catalogue().unwrap()),
            handlers: options.handlers.unwrap_or_else(stub_handlers),
            nonces: options.nonces,
            administrator_keys: vec![signer.public_key_bytes()],
            operator_ca: Vec::new(),
            operators: OperatorAllowList::new(vec![OperatorEntry::new("CN=alice", "alice")]),
            approvals: options.approvals,
            clock,
            audit: audit_sink,
            commands: commands.clone(),
            site_id: SITE.to_owned(),
        };
        Self {
            router: router(deps),
            audit,
            commands,
            signer,
            counter: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A signed envelope body for `op`, valid at [`NOW`].
    pub fn signed(&self, op: &str, params: Value) -> String {
        self.signed_with(op, params, SITE, NOW - 5, 300)
    }

    pub fn signed_with(
        &self,
        op: &str,
        params: Value,
        audience: &str,
        issued_at: i64,
        ttl: i64,
    ) -> String {
        let command = Command::new(
            op,
            params,
            audience,
            self.signer.key_id(),
            issued_at,
            issued_at + ttl,
        );
        let signed = sign(&command, &self.signer).unwrap();
        wire::encode(&signed, &[]).unwrap()
    }

    /// An envelope without signatures.
    pub fn unsigned(&self, op: &str, params: Value) -> String {
        let text = self.signed(op, params);
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value["signatures"] = json!([]);
        value.to_string()
    }

    pub async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
        idempotency: Option<&str>,
        peer: Option<PeerCertificate>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(key) = idempotency {
            builder = builder.header("idempotency-key", key);
        }
        if let Some(peer) = peer {
            builder = builder.extension(peer);
        }
        let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// POST a command as alice with a fresh idempotency key.
    pub async fn invoke(&self, body: String) -> (StatusCode, Value) {
        let key = format!(
            "k-{}",
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        self.send(
            "POST",
            "/mgmt/v1/commands",
            Some(body),
            Some(&key),
            Some(alice()),
        )
        .await
    }

    /// Polls a command until it reaches `state` (or fails the test after about 3 seconds).
    pub async fn wait_state(&self, id: &str, state: &str) -> Value {
        for _ in 0..60 {
            let (status, body) = self
                .send(
                    "GET",
                    &format!("/mgmt/v1/commands/{id}"),
                    None,
                    None,
                    Some(alice()),
                )
                .await;
            assert_eq!(status, StatusCode::OK);
            if body["data"]["state"] == state {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("command {id} never reached {state}");
    }
}

pub fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

pub fn status_params() -> Value {
    json!({ "site": "site-1" })
}

pub fn backup_params() -> Value {
    json!({ "site": "site-1", "target": "local" })
}
