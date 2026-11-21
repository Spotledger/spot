//! [`ConnectorHost`]: loads signed connector components, describes them, invokes their
//! operations and delivers webhooks to them, with a credential vault, a monitor and a circuit
//! breaker. It implements [`ConnectorPort`], the trait the business-process engine drives.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use semver::Version;
use spot_bundle::{Bundle, HostInfo, Limits as BundleLimits};
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, RevocationList, TrustAnchor, VerifyPolicy,
    verify,
};
use wasmtime::component::Component;

use crate::config::ConnectorConfig;
use crate::error::{ConnectorError, Result};
use crate::imports::{
    CallServices, HttpContext, HttpService, InMemoryKv, KvStore, LogSink, NullLogSink,
};
use crate::model::{CallContext, ConnectorId, ConnectorInfo, HttpRequest, HttpResponse};
use crate::monitor::{ConnectorMonitor, Refusal};
use crate::port::{ConnectorCall, ConnectorHealth, ConnectorOutput, ConnectorPort};
use crate::runtime::ConnectorRuntime;
use crate::vault::CredentialVault;

/// A signed connector bundle and the material needed to verify it.
pub struct SignedBundle<'a> {
    /// The bundle bytes (a `spot-bundle` archive).
    pub bytes: &'a [u8],
    /// The detached signature.
    pub signature: &'a BundleSignature,
    /// The publisher's certificate chain.
    pub chain: &'a CertificateChain,
    /// The store's trust anchor.
    pub anchor: &'a TrustAnchor,
    /// The revocation list.
    pub revocations: &'a RevocationList,
    /// The current time (UTC seconds).
    pub now: i64,
    /// The verification policy.
    pub policy: &'a VerifyPolicy,
}

struct Installed {
    component: Component,
    config: Arc<ConnectorConfig>,
    info: ConnectorInfo,
}

/// The host: the runtime, the vault, the transport, the sinks, the monitor and the installed
/// connectors. Clone the [`Arc`] to share it.
pub struct ConnectorHost {
    runtime: Arc<ConnectorRuntime>,
    vault: Arc<dyn CredentialVault>,
    http: Arc<dyn HttpService>,
    logs: Arc<dyn LogSink>,
    kv: Arc<dyn KvStore>,
    monitor: Arc<ConnectorMonitor>,
    connectors: RwLock<HashMap<ConnectorId, Installed>>,
}

impl ConnectorHost {
    /// A host over `runtime`, `vault`, `http` and `monitor`, with a null log sink and an
    /// in-memory key-value store.
    pub fn new(
        runtime: Arc<ConnectorRuntime>,
        vault: Arc<dyn CredentialVault>,
        http: Arc<dyn HttpService>,
        monitor: Arc<ConnectorMonitor>,
    ) -> Self {
        Self {
            runtime,
            vault,
            http,
            logs: Arc::new(NullLogSink),
            kv: Arc::new(InMemoryKv::new()),
            monitor,
            connectors: RwLock::new(HashMap::new()),
        }
    }

    /// Sets the log sink.
    #[must_use]
    pub fn with_logs(mut self, logs: Arc<dyn LogSink>) -> Self {
        self.logs = logs;
        self
    }

    /// Sets the key-value store.
    #[must_use]
    pub fn with_kv(mut self, kv: Arc<dyn KvStore>) -> Self {
        self.kv = kv;
        self
    }

    /// Installs an already-verified component. Its `describe` is called to record its identity.
    pub async fn install(&self, config: ConnectorConfig, wasm: &[u8]) -> Result<ConnectorInfo> {
        let component = self.runtime.compile(wasm)?;
        self.install_compiled(config, component).await
    }

    /// Verifies a signed bundle (`spot-bundle` + `spot-bundle-signing`) and installs it.
    pub async fn install_bundle(
        &self,
        config: ConnectorConfig,
        signed: SignedBundle<'_>,
    ) -> Result<ConnectorInfo> {
        let host = HostInfo {
            version: Version::parse(env!("CARGO_PKG_VERSION"))
                .unwrap_or_else(|_| Version::new(0, 1, 0)),
        };
        let bundle =
            Bundle::open(signed.bytes, &BundleLimits::default(), &host).map_err(|errors| {
                let first = errors.first();
                ConnectorError::BadBundle {
                    reason: first.map_or_else(
                        || "the bundle is invalid".into(),
                        |e| format!("{}: {}", e.code, e.message),
                    ),
                }
            })?;
        let digest = bundle.digest();
        let app_id = bundle.manifest().app.name.to_string();
        let version = bundle.manifest().app.version.to_string();
        let facts = BundleFacts {
            digest: &digest,
            app_id: &app_id,
            version: &version,
        };
        verify(
            &facts,
            signed.signature,
            signed.chain,
            signed.anchor,
            signed.revocations,
            signed.now,
            signed.policy,
        )
        .map_err(|e| ConnectorError::Unverified {
            reason: format!("{}: {e}", e.code()),
        })?;
        let component = self.runtime.compile(bundle.wasm())?;
        self.install_compiled(config, component).await
    }

    async fn install_compiled(
        &self,
        config: ConnectorConfig,
        component: Component,
    ) -> Result<ConnectorInfo> {
        config.validate()?;
        let config = Arc::new(config);
        let services = self.services(Arc::clone(&config), "", None, 0);
        let info = self.runtime.describe(&component, services).await?;
        let mut connectors = self.write()?;
        connectors.insert(
            config.id.clone(),
            Installed {
                component,
                config,
                info: info.clone(),
            },
        );
        Ok(info)
    }

    /// Removes a connector.
    pub fn uninstall(&self, id: &ConnectorId) -> bool {
        self.connectors
            .write()
            .map(|mut map| map.remove(id).is_some())
            .unwrap_or(false)
    }

    /// The connectors currently installed.
    pub fn installed(&self) -> Vec<ConnectorId> {
        self.connectors
            .read()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The monitor, for reporting.
    pub fn monitor(&self) -> &Arc<ConnectorMonitor> {
        &self.monitor
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<ConnectorId, Installed>>> {
        self.connectors
            .read()
            .map_err(|_| ConnectorError::HostFailure {
                reason: "the connector registry is poisoned".into(),
            })
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<ConnectorId, Installed>>> {
        self.connectors
            .write()
            .map_err(|_| ConnectorError::HostFailure {
                reason: "the connector registry is poisoned".into(),
            })
    }

    fn lookup(&self, id: &ConnectorId) -> Result<(Component, Arc<ConnectorConfig>, ConnectorInfo)> {
        let connectors = self.read()?;
        let installed = connectors
            .get(id)
            .ok_or_else(|| ConnectorError::NotFound { id: id.to_string() })?;
        Ok((
            installed.component.clone(),
            Arc::clone(&installed.config),
            installed.info.clone(),
        ))
    }

    fn services(
        &self,
        config: Arc<ConnectorConfig>,
        tenant: &str,
        actor: Option<String>,
        now_micros: i64,
    ) -> Arc<CallServices> {
        Arc::new(CallServices {
            config,
            vault: Arc::clone(&self.vault),
            http: Arc::clone(&self.http),
            logs: Arc::clone(&self.logs),
            kv: Arc::clone(&self.kv),
            monitor: Arc::clone(&self.monitor),
            ctx: HttpContext {
                tenant: tenant.to_owned(),
                actor,
                now_micros,
            },
        })
    }

    /// Invokes one operation, through the breaker.
    pub async fn invoke(&self, call: ConnectorCall) -> Result<ConnectorOutput> {
        let (component, config, _info) = self.lookup(&call.connector)?;
        let permit = self.admit(&call.connector, &call.tenant, call.now_micros)?;
        let input = serde_json::to_string(&call.input).map_err(|_| ConnectorError::BadInput {
            reason: "the input could not be serialised".into(),
        })?;
        let services = self.services(config, &call.tenant, call.actor.clone(), call.now_micros);
        let ctx = CallContext {
            tenant: call.tenant.clone(),
            actor: call.actor.clone(),
            now_micros: call.now_micros,
            deadline_micros: call.deadline_micros,
        };
        let started = Instant::now();
        let result = self
            .runtime
            .invoke(&component, services, &call.operation, &input, ctx)
            .await;
        self.monitor
            .record_latency(&call.connector, "invoke", micros(started));
        match result {
            Ok(text) => {
                permit.record_success("invoke");
                let value = serde_json::from_str(&text).map_err(|_| ConnectorError::BadInput {
                    reason: "the connector returned text that is not JSON".into(),
                })?;
                Ok(ConnectorOutput { value })
            }
            Err(error) => {
                permit.record_failure(&error, "invoke", call.now_micros);
                Err(error)
            }
        }
    }

    /// Delivers an inbound webhook request, through the breaker.
    pub async fn deliver_webhook(
        &self,
        connector: &ConnectorId,
        request: HttpRequest,
        tenant: &str,
        now_micros: i64,
    ) -> Result<HttpResponse> {
        let (component, config, _info) = self.lookup(connector)?;
        let permit = self.admit(connector, tenant, now_micros)?;
        let request_json =
            serde_json::to_string(&request).map_err(|_| ConnectorError::BadRequest {
                reason: "the webhook request could not be serialised".into(),
            })?;
        let services = self.services(config, tenant, None, now_micros);
        let started = Instant::now();
        let result = self
            .runtime
            .webhook(&component, services, &request_json)
            .await;
        self.monitor
            .record_latency(connector, "webhook", micros(started));
        match result {
            Ok(text) => {
                permit.record_success("webhook");
                serde_json::from_str(&text).map_err(|_| ConnectorError::BadRequest {
                    reason: "the connector returned a webhook response that is not JSON".into(),
                })
            }
            Err(error) => {
                permit.record_failure(&error, "webhook", now_micros);
                Err(error)
            }
        }
    }

    fn admit(
        &self,
        connector: &ConnectorId,
        tenant: &str,
        now_micros: i64,
    ) -> Result<crate::monitor::Permit> {
        self.monitor.acquire(connector, tenant, now_micros).map_err(
            |Refusal::CircuitOpen { retry_after_secs }| ConnectorError::CircuitOpen {
                retry_after_secs,
            },
        )
    }
}

fn micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[async_trait]
impl ConnectorPort for ConnectorHost {
    async fn describe(&self, connector: &ConnectorId) -> Result<ConnectorInfo> {
        let (_component, _config, info) = self.lookup(connector)?;
        Ok(info)
    }

    async fn invoke(&self, call: ConnectorCall) -> Result<ConnectorOutput> {
        ConnectorHost::invoke(self, call).await
    }

    async fn webhook(
        &self,
        connector: &ConnectorId,
        request: HttpRequest,
        now_micros: i64,
    ) -> Result<HttpResponse> {
        self.deliver_webhook(connector, request, "", now_micros)
            .await
    }

    async fn ping(&self, connector: &ConnectorId) -> Result<ConnectorHealth> {
        let _ = self.lookup(connector)?;
        let now = 0;
        if self.monitor.is_open(connector, "", now) {
            Ok(ConnectorHealth {
                healthy: false,
                detail: "the circuit breaker is open".into(),
            })
        } else {
            Ok(ConnectorHealth {
                healthy: true,
                detail: "installed and compiled".into(),
            })
        }
    }
}
