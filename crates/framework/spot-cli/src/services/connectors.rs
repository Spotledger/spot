//! The connector service of one site (WP-INT-05.T3; ADR-49): it hosts the connector SDK
//! (`spot-connectors`) so a site can configure, schedule, run and watch connectors.
//!
//! # What it provides
//!
//! - **Definitions** ([`connectors::defs`]): the `connector` DocType row (component digest,
//!   endpoints allow-list, credential *references*, schedule, mapping, state) read through a
//!   directory; the `connector_run` DocType is the read-only run log ([`connectors::runs`]).
//! - **A durable vault** ([`connectors::vault`]): credential values are written once through a
//!   write-only action, sealed with the site's keyring, stored in PostgreSQL keyed by
//!   `(site, reference)`, and opened only inside the host's `secrets-get` import for a reference the
//!   connector declared (or when the host injects an `auth` credential). No DocType field holds a
//!   value, and an operator who can edit a connector cannot read a secret back.
//! - **Runs** ([`connectors::runner`]): the job handler `platform:connector_run`. Scheduled
//!   connectors enqueue one job per slot with the slot in the idempotency key, so two nodes ticking
//!   the same slot create one job; manual runs use [`ConnectorsPort::run_now`]. Limits per connector
//!   come from `[services.connectors.limits]` (capped by site maxima); a retryable failure is
//!   retried with the worker's backoff, and the last failed attempt puts the connector in the
//!   dead-letter state and raises an alert.
//! - **Egress** ([`connectors::egress`]): every outbound call passes `spot-egress` under the
//!   connector's own allow-list; an empty list means no network at all.
//! - **Monitoring**: `spot_connector_runs_total{outcome}`, the host's duration histograms, the age of the
//!   last success in [`ConnectorsPort::status`]; a connector that is `failed` or in dead letter
//!   degrades readiness.
//!
//! # Level
//!
//! Optional. A site with no connector has it idle. If the secrets service is not running the
//! service does not register (a vault without a keyring would be a plaintext store).
//!
//! # Settings (`[services.connectors]`)
//!
//! See `docs/running-spot/connectors.md`.

#[path = "connectors/defs.rs"]
pub mod defs;
#[path = "connectors/egress.rs"]
pub mod egress;
#[path = "connectors/runner.rs"]
pub mod runner;
#[path = "connectors/runs.rs"]
pub mod runs;
#[path = "connectors/vault.rs"]
pub mod vault;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use spot_connectors::{ConnectorHost, ConnectorMonitor, ConnectorRuntime, MonitorConfig};
use spot_core::SiteId;
use spot_jobs::worker::JobContext;
use spot_secrets::SecretString;
use tokio::sync::watch;

use crate::services::apps::store::DirBundleStore;
use crate::services::apps::verify::Trust;
use crate::services::ports::{JobSpec, JobSubmit};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

use defs::{Boxed, PgDirectory};
use runner::{Connectors, Parts, TracingAlerts};
use runs::PgRunLog;
use vault::{DurableVault, PgSealedStore};

pub use runner::ConnectorStatus;

/// Start order: after the secrets (1000), jobs (600) and apps (1100) services.
pub const ORDER: u32 = 1200;

fn d_true() -> bool {
    true
}
fn d_bundles() -> PathBuf {
    PathBuf::from("bundles")
}
fn d_tick() -> u64 {
    15
}
fn d_refresh() -> u64 {
    60
}
fn d_runs() -> u32 {
    1
}
fn d_calls() -> u32 {
    16
}

/// `services.connectors`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host connectors. Default true.
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// The bundle store (the same content-addressed layout as `services.apps`).
    #[serde(default = "d_bundles")]
    pub bundles_dir: PathBuf,
    /// Seconds between schedule ticks. 1 to 3,600.
    #[serde(default = "d_tick")]
    pub schedule_tick_seconds: u64,
    /// Seconds between re-reads of the definitions. 5 to 86,400.
    #[serde(default = "d_refresh")]
    pub refresh_seconds: u64,
    /// Runs of one connector at once. 1 to 64.
    #[serde(default = "d_runs")]
    pub max_concurrent_runs: u32,
    /// Calls the sandbox runs at once. 1 to 1,024.
    #[serde(default = "d_calls")]
    pub max_concurrent_calls: u32,
    /// Sandbox limits (per-connector overrides under `limits.apps.<name>`).
    #[serde(default)]
    pub limits: crate::services::apps::limits::LimitTable,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            bundles_dir: d_bundles(),
            schedule_tick_seconds: d_tick(),
            refresh_seconds: d_refresh(),
            max_concurrent_runs: d_runs(),
            max_concurrent_calls: d_calls(),
            limits: crate::services::apps::limits::LimitTable::default(),
        }
    }
}

impl Settings {
    /// The first setting out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=3_600).contains(&self.schedule_tick_seconds) {
            return Err("schedule_tick_seconds");
        }
        if !(5..=86_400).contains(&self.refresh_seconds) {
            return Err("refresh_seconds");
        }
        if !(1..=64).contains(&self.max_concurrent_runs) {
            return Err("max_concurrent_runs");
        }
        if !(1..=1_024).contains(&self.max_concurrent_calls) {
            return Err("max_concurrent_calls");
        }
        self.limits.validate()
    }
}

/// Writes secrets and re-seals them. Nothing here returns a value.
pub trait SecretAdmin: Send + Sync {
    /// Stores `value` under `reference`, replacing any.
    fn put<'a>(
        &'a self,
        reference: &'a str,
        value: &'a SecretString,
    ) -> Boxed<'a, std::result::Result<(), String>>;
    /// Re-seals every value under the current key; returns how many.
    fn reseal_all(&self) -> Boxed<'_, std::result::Result<usize, String>>;
    /// Whether a value exists under `reference`.
    fn contains<'a>(&'a self, reference: &'a str) -> Boxed<'a, std::result::Result<bool, String>>;
}

impl<P: spot_secrets::KeyProvider + 'static> SecretAdmin for DurableVault<P> {
    fn put<'a>(
        &'a self,
        reference: &'a str,
        value: &'a SecretString,
    ) -> Boxed<'a, std::result::Result<(), String>> {
        Box::pin(DurableVault::put(self, reference, value))
    }

    fn reseal_all(&self) -> Boxed<'_, std::result::Result<usize, String>> {
        Box::pin(DurableVault::reseal_all(self))
    }

    fn contains<'a>(&'a self, reference: &'a str) -> Boxed<'a, std::result::Result<bool, String>> {
        Box::pin(DurableVault::contains(self, reference))
    }
}

/// What the routes (WP-INT-05.T4) and other modules take from the service.
pub trait ConnectorsPort: Send + Sync {
    /// Every connector and its state.
    fn status(&self) -> Vec<ConnectorStatus>;
    /// Queues one manual run; `true` when it is new.
    fn run_now<'a>(
        &'a self,
        connector: &'a str,
        operation: &'a str,
        input: Value,
    ) -> Boxed<'a, std::result::Result<bool, String>>;
    /// The write-only secret actions.
    fn secrets(&self) -> Arc<dyn SecretAdmin>;
    /// Re-reads the definitions and reloads the connectors.
    fn refresh(&self) -> Boxed<'_, std::result::Result<(), String>>;
}

/// The hosted service.
pub struct ConnectorsService {
    settings: Settings,
    engine: Arc<Connectors>,
    submit: Arc<dyn JobSubmit>,
    admin: Arc<dyn SecretAdmin>,
    clock: Arc<dyn spot_core::Clock>,
}

impl ConnectorsService {
    /// A service over its parts.
    pub fn new(
        settings: Settings,
        engine: Arc<Connectors>,
        submit: Arc<dyn JobSubmit>,
        admin: Arc<dyn SecretAdmin>,
        clock: Arc<dyn spot_core::Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            engine,
            submit,
            admin,
            clock,
        })
    }

    fn now_secs(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Queues the run of every scheduled connector whose slot has come. The slot number is in the
    /// idempotency key, so any number of nodes ticking the same slot create one job.
    ///
    /// # Errors
    ///
    /// The first job-store failure (the rest of the tick still ran).
    pub async fn schedule_tick(&self) -> std::result::Result<usize, String> {
        let now = self.now_secs();
        let mut queued = 0;
        let mut failure = None;
        for def in self.engine.scheduled() {
            let Some(schedule) = &def.schedule else {
                continue;
            };
            let every = i64::try_from(schedule.every_seconds.max(10)).unwrap_or(i64::MAX);
            let slot = now / every;
            let spec = JobSpec {
                name: runner::JOB_NAME.to_owned(),
                queue: "default".to_owned(),
                args: Connectors::job_args(&def.name, &schedule.operation, &schedule.input),
                idempotency_key: format!("connector:{}:{slot}", def.name),
                not_before: None,
            };
            match self.submit.submit(spec).await {
                Ok(true) => queued += 1,
                Ok(false) => {}
                Err(e) => failure = Some(e.to_string()),
            }
        }
        failure.map_or(Ok(queued), Err)
    }
}

impl ConnectorsPort for ConnectorsService {
    fn status(&self) -> Vec<ConnectorStatus> {
        self.engine.status()
    }

    fn run_now<'a>(
        &'a self,
        connector: &'a str,
        operation: &'a str,
        input: Value,
    ) -> Boxed<'a, std::result::Result<bool, String>> {
        Box::pin(async move {
            let nonce = self.clock.now_utc().unix_timestamp_nanos();
            let spec = JobSpec {
                name: runner::JOB_NAME.to_owned(),
                queue: "default".to_owned(),
                args: Connectors::job_args(connector, operation, &input),
                idempotency_key: format!("connector:{connector}:manual:{nonce}"),
                not_before: None,
            };
            self.submit.submit(spec).await.map_err(|e| e.to_string())
        })
    }

    fn secrets(&self) -> Arc<dyn SecretAdmin> {
        Arc::clone(&self.admin)
    }

    fn refresh(&self) -> Boxed<'_, std::result::Result<(), String>> {
        Box::pin(self.engine.refresh())
    }
}

impl Service for ConnectorsService {
    fn name(&self) -> &'static str {
        "connectors"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let degraded = self.engine.degradation();
            if degraded.is_empty() {
                Ok(())
            } else {
                Err(degraded.join("; "))
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let tick = Duration::from_secs(self.settings.schedule_tick_seconds);
            let refresh_every =
                (self.settings.refresh_seconds / self.settings.schedule_tick_seconds).max(1);
            let mut n: u64 = 0;
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = tokio::time::sleep(tick) => {}
                }
                n += 1;
                if n.is_multiple_of(refresh_every) {
                    let _ = self.engine.refresh().await;
                }
                if let Err(e) = self.schedule_tick().await {
                    tracing::warn!(error = %e, "connector schedule tick failed");
                }
            }
        })
    }
}

/// Registers the connector service: vault, host, directory, run log, the job handler and the port.
///
/// # Errors
///
/// Settings out of range, a missing secrets or jobs service, or a table that cannot be prepared.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("connectors")?;
    if let Err(key) = settings.validate() {
        return Err(Error(format!("services.connectors.{key}: out of range")));
    }
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    SiteId::new(&site_name).map_err(|e| Error(format!("services.connectors: {e}")))?;
    let secrets = ctx.port::<super::secrets::SecretsHost>().ok_or_else(|| {
        Error(
            "services.connectors: the secrets service must be running (no keyring to seal with)"
                .to_owned(),
        )
    })?;
    let submit = ctx.require_port::<dyn JobSubmit>()?;
    let clock = Arc::clone(ctx.clock());

    let pool = env.backend.pool();
    let directory = PgDirectory::new(pool.clone());
    let run_log = PgRunLog::new(pool.clone());
    let sealed = PgSealedStore::new(pool);
    for step in [
        directory.prepare().await,
        run_log.prepare().await,
        sealed.prepare().await,
    ] {
        step.map_err(|code| Error(format!("services.connectors: {code}")))?;
    }

    let vault = Arc::new(DurableVault::new(
        &site_name,
        Arc::clone(secrets.keyring()),
        Arc::new(sealed),
    ));
    let http = Arc::new(egress::PerConnectorHttp::new(
        &site_name,
        Arc::new(spot_egress::SystemResolver),
        Arc::clone(&clock),
    ));
    let runtime = ConnectorRuntime::new(spot_wasm::HostConfig {
        max_concurrent_calls: settings.max_concurrent_calls,
        max_memory_bytes: settings.limits.max_memory_bytes(),
        ..spot_wasm::HostConfig::default()
    })
    .map_err(|e| Error(format!("services.connectors: {}", e.code())))?;
    let host = Arc::new(ConnectorHost::new(
        Arc::new(runtime),
        Arc::clone(&vault) as Arc<dyn spot_connectors::CredentialVault>,
        Arc::clone(&http) as Arc<dyn spot_connectors::HttpService>,
        Arc::new(ConnectorMonitor::new(MonitorConfig::default())),
    ));
    let trust = Trust::load(
        env.root,
        env.loaded.common.trust.roots.as_deref(),
        env.loaded.common.trust.revocations.as_deref(),
        env.loaded.common.trust.require_signed,
    );
    let engine = Arc::new(Connectors::new(Parts {
        site: site_name,
        host,
        http,
        directory: Arc::new(directory),
        runs: Arc::new(run_log),
        alerts: Arc::new(TracingAlerts),
        store: Arc::new(DirBundleStore::new(env.root.join(&settings.bundles_dir))),
        trust: Arc::new(trust),
        clock: Arc::clone(&clock),
        limits: settings.limits.clone(),
        max_concurrent_runs: settings.max_concurrent_runs,
    }));
    // A directory that cannot be read leaves nothing loaded; the next refresh tries again.
    let _ = engine.refresh().await;

    let handler_engine = Arc::clone(&engine);
    ctx.job_handlers()
        .register(runner::JOB_NAME, move |job: JobContext| {
            let engine = Arc::clone(&handler_engine);
            Box::pin(async move {
                Ok(engine
                    .run_job(&job.job.args, job.attempt, job.is_last_attempt())
                    .await)
            })
        });

    let service = ConnectorsService::new(settings, engine, submit, vault, clock);
    ctx.provide::<dyn ConnectorsPort>(Arc::clone(&service) as Arc<dyn ConnectorsPort>)?;
    ctx.add(service as Arc<dyn Service>);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_unknown_keys_are_refused() {
        assert!(Settings::default().validate().is_ok());
        let bad: std::result::Result<Settings, _> = toml::from_str("nope = 1");
        assert!(bad.is_err());
        let zero = Settings {
            max_concurrent_runs: 0,
            ..Settings::default()
        };
        assert_eq!(zero.validate(), Err("max_concurrent_runs"));
    }
}
