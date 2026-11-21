//! The LLM service (WP-AI-03.T5; blueprint WS13, ADR-26): bring-your-own-key model providers
//! hosted behind the task router, under the tenant's policy. Operator guide:
//! `docs/running-spot/llm.md`.
//!
//! # What it does
//!
//! `spot-llm`, its two providers and `spot-llm-router` are libraries. This module hosts them so
//! server code, and later agents, can call a model:
//!
//! - **Per-tenant provider configuration** (kind, endpoint, model, allow-list, price, budget) in
//!   `spot_llm_tenant`. The **key is write-only**: [`gateway::LlmGateway::set_key`] seals it with
//!   the secrets vault, bound to the site and tenant; no call returns it; it is opened only on the
//!   call path, into a zeroising `ApiKey`.
//! - **The allow-list**: an endpoint outside the tenant's `allowed_hosts` is refused before any
//!   lookup or byte; the operator's egress policy (`purpose = llm`) is checked as well, after DNS
//!   resolution, so a name that resolves to a private or metadata address is refused with no
//!   connection, and the transport authorises every redirect hop again. Loopback and private
//!   hosts (a local model) work only when the egress policy lists them with `allow_private`.
//! - **Durable usage and a budget stop**: tokens and cost (integer micro-units) per tenant, task,
//!   day, provider and model in `spot_llm_usage`; a tenant budget per day or month. A call whose
//!   worst case would pass the cap is refused before the provider is touched, and the spend of
//!   the period is read back from the database, so a restart does not reset it.
//! - **The record of what was sent** (`spot_llm_send`): per call the endpoint (scheme, host,
//!   port), model, a SHA-256 digest of the request, its size, the DocTypes and the number of
//!   fields of each class, the user and the purpose, and the outcome. Never the content and never
//!   the key. It is written *before* the provider is called, and kept for
//!   `services.llm.retention_days` (default 90).
//! - **Redaction**: [`prompt::PromptBuilder`] strips protected and sealed fields (and personal ones
//!   unless the policy includes them) and redacts what remains with `spot-redact`; a document
//!   enters a prompt only through the permission-checked read as the invoking user
//!   ([`prompt::DocumentReader`]: the engine-backed one the `llm_reader` module provides, and a
//!   reader that denies everything when a site has none).
//!
//! # Embeddings (WP-AI-03.T7)
//!
//! [`gateway::LlmGateway::embed`] embeds texts with a tenant's `embedding_model` through the same
//! controls as a completion: the allow-list, the egress policy, the key, the budget (a worst case
//! reserved before the call, the exact tokens settled after), a send record written first (digest,
//! size, counts and purpose, never a text) and usage counted under the task `embed`. A text the
//! redactor flags as a secret or a financial identifier is refused, not sent. The module provides
//! the two ports that need it, both always, so a site without an embedding setup reports a precise
//! reason instead of "no embedding provider":
//!
//! - `dyn retrieval::ports::EmbedPort` ([`embed_port::GatewayEmbedPort`]): the tenant named by
//!   `[services.retrieval] provider`; the indexer runs as the named caller `system-indexer`, a
//!   query as the user or agent that asked.
//! - `dyn spot_agent_memory::Embedder` ([`embed_sync::GatewayEmbedder`]): synchronous, for agent
//!   memory's blocking threads; the tenant named by `embedding_tenant`. A configured width that
//!   differs from the memory store's is a degraded sentence at start naming both numbers, and
//!   the port is then not provided.
//!
//! With no tenant set for embeddings every call answers `llm_host.embeddings_not_configured`.
//!
//! # Settings (`[services.llm]`)
//!
//! `enabled` (true), `max_retries` (2), `retention_days` (90), `maintenance_seconds` (30),
//! `request_timeout_seconds` (120), `max_texts_per_call` (512), `max_text_bytes` (32768),
//! `embedding_tenant` (none) and `[[services.llm.tenants]]` entries that seed tenant
//! configurations at start (never a key). Unknown keys and invalid entries stop the site from
//! starting with a message naming the key.
//!
//! # Level
//!
//! **Optional.** If the tables cannot be created, or the secrets host or the egress service is not
//! running, the site serves and readiness says `degraded: llm (...)`; no gateway port is provided,
//! so no model call can be made. Model calls are never made without the guard.
//!
//! # Ports
//!
//! Provides [`gateway::LlmGateway`], `dyn EmbedPort` and `dyn Embedder`; takes `SecretsHost`,
//! `SharedGuard` and (optionally) `dyn DocumentReader`. Its [`ORDER`] stays below the retrieval
//! service's (1270) and agent memory's (1280), which read the embedding ports.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_redact::{RedactionPolicy, Redactor};
use tokio::sync::watch;

use crate::services::egress::SharedGuard;
use crate::services::retrieval::ports::EmbedPort;
use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "llm/config.rs"]
pub mod config;
#[path = "llm/embed_port.rs"]
pub mod embed_port;
#[path = "llm/embed_sync.rs"]
pub mod embed_sync;
#[path = "llm/gateway.rs"]
pub mod gateway;
#[path = "llm/prompt.rs"]
pub mod prompt;
#[path = "llm/store.rs"]
pub mod store;
#[path = "llm/usage.rs"]
pub mod usage;

use config::TenantConfig;
use gateway::{BuiltinProviders, Deps, EgressTransports, EmbedLimits, LlmGateway};
use prompt::{DenyAllReader, DocumentReader};
use spot_agent_memory::{EMBEDDING_DIM, Embedder};
use store::Store;

/// Start order: after the secrets host (1000) and the egress service (950) whose ports it takes.
pub const ORDER: u32 = 1260;

/// Catalogue codes that head the sentences of settings and degraded reports.
const SETTINGS_INVALID: &str = "llm_host.settings_invalid";
const SECRETS_UNAVAILABLE: &str = "llm_host.secrets_unavailable";
const EGRESS_UNAVAILABLE: &str = "llm_host.egress_unavailable";

fn default_true() -> bool {
    true
}

fn default_retries() -> u32 {
    2
}

fn default_retention() -> u32 {
    90
}

fn default_maintenance() -> u64 {
    30
}

fn default_timeout() -> u64 {
    120
}

fn default_max_texts() -> usize {
    512
}

fn default_max_text_bytes() -> usize {
    32_768
}

/// `[services.llm]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host the service. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Retries of a retryable provider failure on the same route.
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Days a send record is kept.
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    /// Seconds between usage flushes and retention sweeps.
    #[serde(default = "default_maintenance")]
    pub maintenance_seconds: u64,
    /// The total time one model request may take.
    #[serde(default = "default_timeout")]
    pub request_timeout_seconds: u64,
    /// The most texts one embedding call may carry (above the provider batch size, the call is
    /// split into batches).
    #[serde(default = "default_max_texts")]
    pub max_texts_per_call: usize,
    /// The longest single text of an embedding call, in bytes.
    #[serde(default = "default_max_text_bytes")]
    pub max_text_bytes: usize,
    /// The tenant whose embedding setup answers agent memory's synchronous embedder. Unset: that
    /// port answers `llm_host.embeddings_not_configured`.
    #[serde(default)]
    pub embedding_tenant: Option<String>,
    /// Tenant configurations to store at start (keys are never configured here).
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: default_retries(),
            retention_days: default_retention(),
            maintenance_seconds: default_maintenance(),
            request_timeout_seconds: default_timeout(),
            max_texts_per_call: default_max_texts(),
            max_text_bytes: default_max_text_bytes(),
            embedding_tenant: None,
            tenants: Vec::new(),
        }
    }
}

impl Settings {
    /// Checks the numbers and every seeded tenant.
    ///
    /// # Errors
    ///
    /// A refusal naming the key; it never echoes a value.
    pub fn check(&self) -> Result<()> {
        let refuse = |text: &str| Error(format!("{SETTINGS_INVALID}: {text}"));
        if self.max_retries > 10 {
            return Err(refuse("max_retries must be 0 to 10"));
        }
        if !(1..=3_650).contains(&self.retention_days) {
            return Err(refuse("retention_days must be 1 to 3650"));
        }
        if !(1..=3_600).contains(&self.maintenance_seconds) {
            return Err(refuse("maintenance_seconds must be 1 to 3600"));
        }
        if !(1..=3_600).contains(&self.request_timeout_seconds) {
            return Err(refuse("request_timeout_seconds must be 1 to 3600"));
        }
        if !(1..=10_000).contains(&self.max_texts_per_call) {
            return Err(refuse("max_texts_per_call must be 1 to 10000"));
        }
        if !(1..=32_768).contains(&self.max_text_bytes) {
            return Err(refuse("max_text_bytes must be 1 to 32768"));
        }
        if let Some(tenant) = &self.embedding_tenant
            && config::check_tenant(tenant).is_err()
        {
            return Err(refuse("embedding_tenant is not a valid tenant id"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for tenant in &self.tenants {
            if let Err(problem) = tenant.validate() {
                return Err(Error(format!(
                    "{SETTINGS_INVALID}: tenants entry `{}`: {problem}",
                    tenant.tenant
                )));
            }
            if seen.contains(&tenant.tenant.as_str()) {
                return Err(refuse("a tenant is configured twice"));
            }
            seen.push(&tenant.tenant);
        }
        Ok(())
    }
}

/// The service: checks the tables, and on a timer writes pending usage and sweeps old records.
struct LlmService {
    gateway: Arc<LlmGateway>,
    interval: Duration,
}

impl Service for LlmService {
    fn name(&self) -> &'static str {
        "llm"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.gateway.probe().await.map_err(|e| e.to_string()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(self.interval);
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tick.tick() => {
                        self.gateway.flush_all().await;
                        if self.gateway.purge_old().await.is_err() {
                            tracing::warn!("llm: the retention sweep failed; it runs again");
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.gateway.flush_all().await;
            Ok(())
        })
    }
}

/// A degraded entry: the module is present but cannot serve, for `reason`.
fn degraded(ctx: &mut SiteCtx<'_>, reason: impl Into<String>) {
    degraded_as(ctx, "llm", reason);
}

/// A degraded entry under `name`, for a part of the module that cannot serve.
fn degraded_as(ctx: &mut SiteCtx<'_>, name: &'static str, reason: impl Into<String>) {
    let reason: String = reason.into();
    ctx.add(Idle::new(name, Level::Optional, move || {
        let reason = reason.clone();
        Box::pin(async move { Err(reason) })
    }));
}

/// Registers the LLM service: its tables, its readiness entry and the [`LlmGateway`] port.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("llm")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    settings.check()?;
    let Some(secrets) = ctx.port::<SecretsHost>() else {
        degraded(
            ctx,
            format!(
                "{SECRETS_UNAVAILABLE}: no secrets host is configured, so keys cannot be sealed"
            ),
        );
        return Ok(());
    };
    let Some(guard) = ctx.port::<SharedGuard>() else {
        degraded(
            ctx,
            format!(
                "{EGRESS_UNAVAILABLE}: the egress service is not running, and a model call is never made without it"
            ),
        );
        return Ok(());
    };
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let store = Store::pg(env.backend.pool(), &site);
    if store.prepare().await.is_err() {
        degraded(
            ctx,
            "llm_host.store_unavailable: the LLM tables could not be created",
        );
        return Ok(());
    }
    let redactor = Redactor::from_policy(&RedactionPolicy::default()).map_err(|_| {
        Error(format!(
            "{SETTINGS_INVALID}: the redaction rules did not compile"
        ))
    })?;
    let reader: Arc<dyn DocumentReader> = ctx
        .port::<dyn DocumentReader>()
        .unwrap_or_else(|| Arc::new(DenyAllReader));
    let gateway = Arc::new(
        LlmGateway::new(Deps {
            site: site.clone(),
            store: store.clone(),
            vault: secrets.vault(),
            guard,
            clock: Arc::clone(ctx.clock()),
            providers: Arc::new(BuiltinProviders),
            transports: Arc::new(EgressTransports {
                total_timeout: Duration::from_secs(settings.request_timeout_seconds),
            }),
            redactor: Arc::new(redactor),
            reader,
            max_retries: settings.max_retries,
            retention_days: settings.retention_days,
        })
        .with_embed_limits(EmbedLimits {
            max_texts: settings.max_texts_per_call,
            max_text_bytes: settings.max_text_bytes,
        }),
    );
    for tenant in &settings.tenants {
        gateway
            .put_config(tenant)
            .await
            .map_err(|e| Error(format!("{SETTINGS_INVALID}: {e}")))?;
    }
    ctx.add(Arc::new(LlmService {
        gateway: Arc::clone(&gateway),
        interval: Duration::from_secs(settings.maintenance_seconds),
    }));
    provide_embedding_ports(ctx, &gateway, &settings).await?;
    ctx.provide(gateway)
        .map_err(|e| Error(format!("services.llm: {e}")))?;
    Ok(())
}

/// Provides the embedding ports (WP-AI-03.T7). Both are always provided, so a consumer without
/// an embedding setup gets `llm_host.embeddings_not_configured` per call instead of a missing
/// port; the one exception is a configured width that differs from what agent memory's store was
/// prepared with, which is reported here, at start, and withholds the synchronous port.
async fn provide_embedding_ports(
    ctx: &mut SiteCtx<'_>,
    gateway: &Arc<LlmGateway>,
    settings: &Settings,
) -> Result<()> {
    let port: Arc<dyn EmbedPort> = Arc::new(embed_port::GatewayEmbedPort::new(Arc::clone(gateway)));
    ctx.provide::<dyn EmbedPort>(port)
        .map_err(|e| Error(format!("services.llm: {e}")))?;

    if let Some(tenant) = settings.embedding_tenant.as_deref() {
        // The tenant's stored configuration (seeded above, or entered with `spot llm`).
        let configured = match gateway.config(tenant).await {
            Ok(config) => config.and_then(|c| c.embedding_dimensions),
            Err(_) => None,
        };
        if let Err(sentence) = embed_sync::check_width(tenant, configured, EMBEDDING_DIM) {
            degraded_as(ctx, "llm_embed", sentence);
            return Ok(());
        }
    }
    let embedder: Arc<dyn Embedder> = Arc::new(embed_sync::GatewayEmbedder::new(
        Arc::clone(gateway),
        settings.embedding_tenant.clone(),
        EMBEDDING_DIM,
        tokio::runtime::Handle::current(),
    ));
    ctx.provide::<dyn Embedder>(embedder)
        .map_err(|e| Error(format!("services.llm: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_and_valid() {
        let s = Settings::default();
        assert!(s.enabled);
        assert!(s.check().is_ok());
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed.retention_days, 90);
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
    }

    #[test]
    fn the_embedding_settings_are_checked() {
        let defaults = Settings::default();
        assert_eq!(
            (defaults.max_texts_per_call, defaults.max_text_bytes),
            (512, 32_768)
        );
        assert!(defaults.embedding_tenant.is_none());
        for bad in [
            Settings {
                max_texts_per_call: 0,
                ..Settings::default()
            },
            Settings {
                max_texts_per_call: 10_001,
                ..Settings::default()
            },
            Settings {
                max_text_bytes: 0,
                ..Settings::default()
            },
            Settings {
                max_text_bytes: 32_769,
                ..Settings::default()
            },
            Settings {
                embedding_tenant: Some("Not A Tenant".to_owned()),
                ..Settings::default()
            },
        ] {
            let error = bad.check().unwrap_err();
            assert!(error.0.contains("llm_host.settings_invalid"), "{}", error.0);
        }
        let parsed: Settings = toml::from_str(
            "max_texts_per_call = 64\nmax_text_bytes = 1000\nembedding_tenant = \"acme\"\n",
        )
        .unwrap();
        assert!(parsed.check().is_ok());
        assert_eq!(parsed.embedding_tenant.as_deref(), Some("acme"));
    }

    #[test]
    fn out_of_range_numbers_and_bad_tenants_are_refused() {
        let s = Settings {
            retention_days: 0,
            ..Settings::default()
        };
        assert!(s.check().is_err());
        let parsed: Settings = toml::from_str(
            "[[tenants]]\ntenant = \"a\"\nkind = \"ollama\"\nendpoint = \"http://h.test\"\nmodel = \"m\"\nallowed_hosts = [\"other.test\"]\n",
        )
        .unwrap();
        let error = parsed.check().unwrap_err();
        assert!(error.0.contains("llm_host.settings_invalid"));
        assert!(!error.0.contains("h.test"));
    }
}
