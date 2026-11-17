//! The connector service's engine: loading connectors, running them, retrying, dead-lettering.
//!
//! # Loading
//!
//! [`Connectors::refresh`] lists the site's connector definitions, reads each bundle by digest from
//! the bundle store, **verifies it** (digest, signature, revocation: the same check the app
//! runtime makes at start-up), installs the component into the connector host with the connector's
//! own configuration (auth reference, declared secrets, endpoints, limits) and gives it its own
//! egress allow-list. A connector that cannot load is `failed` with a code; the rest still run.
//!
//! # Running
//!
//! A run is the job `platform:connector_run` with the arguments `{"connector","operation","input"}`.
//! It runs as the service principal `connector:<name>`: the identity is that connector's, never the
//! administrator who configured it. Failure handling:
//!
//! - a retryable upstream failure (`FailureClass::Retryable`) returns `Retry`, and the job worker
//!   backs off between attempts;
//! - on the **last** attempt the connector goes to the dead-letter state, an alert is raised, and
//!   it runs no more until an operator re-enables it;
//! - a permanent failure fails the job at once and is logged `failed`.
//!
//! Concurrency per connector is capped by a semaphore (`max_concurrent_runs`); a run that finds it
//! full is retried. Nothing here keeps a secret: a run record holds identifiers, an outcome, a
//! duration and a stable code.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{Value, json};
use spot_connectors::error::FailureClass;
use spot_connectors::{
    AuthKind, AuthSpec, ConnectorCall, ConnectorConfig, ConnectorHost, ConnectorId, CredentialRef,
};
use spot_core::Clock;
use spot_jobs::worker::HandlerResult;
use tokio::sync::Semaphore;

use super::defs::{ConnectorDef, ConnectorDirectory, State};
use super::egress::{ACTOR_PREFIX, PerConnectorHttp};
use super::runs::{Outcome, RunLog, RunRecord};
use crate::services::apps::limits::LimitTable;
use crate::services::apps::store::BundleStore;
use crate::services::apps::verify::{self, Trust};

/// The job a run is queued under.
pub const JOB_NAME: &str = "platform:connector_run";
/// `connector.disabled`: the connector is disabled, failed or in the dead-letter state.
pub const DISABLED: &str = "connector.disabled";
/// `connector.busy`: the connector's concurrency cap is reached; the run is retried.
pub const BUSY: &str = "connector.busy";
/// `connector.bundle_invalid`: the connector's bundle is missing, tampered with, revoked or does
/// not describe.
pub const BUNDLE_INVALID: &str = "connector.bundle_invalid";
/// `connector.dead_letter`: the last attempt failed; the connector waits for an operator.
pub const DEAD_LETTER: &str = "connector.dead_letter";
/// `connector.timeout`: the run outlived its deadline.
pub const TIMEOUT: &str = "connector.timeout";
/// `connector.bad_job`: the job's arguments do not name a connector and an operation.
pub const BAD_JOB: &str = "connector.bad_job";

/// Where a dead-letter alert goes.
pub trait AlertSink: Send + Sync {
    /// A connector reached the dead-letter state.
    fn dead_letter(&self, site: &str, connector: &str, code: &str);
}

/// Logs the alert at error level on `spot::alert::connector`.
pub struct TracingAlerts;

impl AlertSink for TracingAlerts {
    fn dead_letter(&self, site: &str, connector: &str, code: &str) {
        tracing::error!(target: "spot::alert::connector", site, connector, code, "connector in dead letter");
    }
}

/// What the status listing says about one connector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorStatus {
    /// The connector.
    pub name: String,
    /// Its state.
    pub state: State,
    /// The load failure code, when it is `failed`.
    pub failure: Option<String>,
    /// Seconds since its last successful run, when it has had one.
    pub last_success_age_secs: Option<u64>,
}

/// The service's parts.
pub struct Parts {
    /// The site.
    pub site: String,
    /// The connector host (SDK).
    pub host: Arc<ConnectorHost>,
    /// The per-connector egress router the host sends through.
    pub http: Arc<PerConnectorHttp>,
    /// Lists the definitions and keeps their state.
    pub directory: Arc<dyn ConnectorDirectory>,
    /// The run log.
    pub runs: Arc<dyn RunLog>,
    /// The alert sink.
    pub alerts: Arc<dyn AlertSink>,
    /// The bundle store.
    pub store: Arc<dyn BundleStore>,
    /// The site's trust.
    pub trust: Arc<Trust>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// Sandbox limits.
    pub limits: LimitTable,
    /// Runs of one connector at once.
    pub max_concurrent_runs: u32,
}

/// The engine of one site.
pub struct Connectors {
    parts: Parts,
    defs: RwLock<BTreeMap<String, ConnectorDef>>,
    load_errors: Mutex<BTreeMap<String, &'static str>>,
    gates: Mutex<HashMap<String, Arc<Semaphore>>>,
    last_ok: Mutex<HashMap<String, i64>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn auth_kind(text: &str) -> Option<AuthKind> {
    match text {
        "bearer" => Some(AuthKind::Bearer),
        "basic" => Some(AuthKind::Basic),
        "oauth2" => Some(AuthKind::OAuth2),
        _ => None,
    }
}

impl Connectors {
    /// An engine over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        Self {
            parts,
            defs: RwLock::new(BTreeMap::new()),
            load_errors: Mutex::new(BTreeMap::new()),
            gates: Mutex::new(HashMap::new()),
            last_ok: Mutex::new(HashMap::new()),
        }
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.parts.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }

    /// The host configuration of one definition, or the code it cannot be configured with.
    fn config_for(&self, def: &ConnectorDef) -> Result<ConnectorConfig, &'static str> {
        let id = ConnectorId::new(def.name.clone()).ok_or(BUNDLE_INVALID)?;
        let mut config = ConnectorConfig::new(id);
        config.limits = self.parts.limits.for_app(&def.name);
        config.endpoints = def.endpoints.clone();
        config.secrets = def
            .secrets
            .iter()
            .filter_map(|s| CredentialRef::new(s.clone()))
            .collect();
        if let Some(auth) = &def.auth {
            let kind = auth_kind(&auth.kind).ok_or(BUNDLE_INVALID)?;
            let credential = CredentialRef::new(auth.credential.clone()).ok_or(BUNDLE_INVALID)?;
            config.auth = Some(AuthSpec {
                kind,
                credential,
                username: auth.username.clone(),
                header: None,
            });
        }
        config.validate().map_err(|_| BUNDLE_INVALID)?;
        Ok(config)
    }

    async fn load_one(&self, def: &ConnectorDef) -> Result<(), &'static str> {
        let stored = self
            .parts
            .store
            .read(&def.digest)
            .map_err(|_| BUNDLE_INVALID)?;
        let now = self.parts.clock.now_utc().unix_timestamp();
        let verified = verify::verify(&stored, &def.digest, &self.parts.trust, now)
            .map_err(|_| BUNDLE_INVALID)?;
        if verified.wasm.is_empty() {
            return Err(BUNDLE_INVALID);
        }
        self.install_verified(def.clone(), &verified.wasm).await
    }

    /// Installs a component whose bundle was already verified, and records `def` as the
    /// connector's definition. [`Connectors::refresh`] calls it after the check; a host that
    /// verifies elsewhere, and the tests, call it directly.
    ///
    /// # Errors
    ///
    /// `connector.bundle_invalid` when the definition or the component is refused.
    pub async fn install_verified(
        &self,
        def: ConnectorDef,
        wasm: &[u8],
    ) -> Result<(), &'static str> {
        let config = self.config_for(&def)?;
        self.parts.http.set_endpoints(&def.name, &def.endpoints);
        self.parts
            .host
            .install(config, wasm)
            .await
            .map_err(|_| BUNDLE_INVALID)?;
        self.defs
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(def.name.clone(), def);
        Ok(())
    }

    /// Re-reads the directory and (re)loads every enabled connector.
    ///
    /// # Errors
    ///
    /// `connector.store_failed` when the directory cannot be read.
    pub async fn refresh(&self) -> Result<(), String> {
        let list = self.parts.directory.list(&self.parts.site).await?;
        let mut defs = BTreeMap::new();
        let mut errors = BTreeMap::new();
        let known: Vec<ConnectorId> = self.parts.host.installed();
        for def in list {
            if def.state == State::Enabled {
                if let Err(code) = self.load_one(&def).await {
                    errors.insert(def.name.clone(), code);
                }
            } else {
                self.drop_installed(&def.name);
            }
            defs.insert(def.name.clone(), def);
        }
        for id in known {
            if !defs.contains_key(id.as_str()) {
                self.drop_installed(id.as_str());
            }
        }
        *self.defs.write().unwrap_or_else(|p| p.into_inner()) = defs;
        *lock(&self.load_errors) = errors;
        Ok(())
    }

    fn drop_installed(&self, name: &str) {
        if let Some(id) = ConnectorId::new(name.to_owned()) {
            self.parts.host.uninstall(&id);
        }
        self.parts.http.remove(name);
    }

    /// Every connector and what became of it.
    #[must_use]
    pub fn status(&self) -> Vec<ConnectorStatus> {
        let now = self.now_micros();
        let errors = lock(&self.load_errors).clone();
        let last = lock(&self.last_ok).clone();
        self.defs
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|def| {
                let failure = errors.get(&def.name).map(|c| (*c).to_owned());
                ConnectorStatus {
                    name: def.name.clone(),
                    state: if failure.is_some() {
                        State::Failed
                    } else {
                        def.state
                    },
                    failure,
                    last_success_age_secs: last
                        .get(&def.name)
                        .map(|at| u64::try_from((now - at) / 1_000_000).unwrap_or(0)),
                }
            })
            .collect()
    }

    /// Why the site is degraded: each connector that failed to load or is in dead letter.
    #[must_use]
    pub fn degradation(&self) -> Vec<String> {
        self.status()
            .into_iter()
            .filter_map(|s| match s.state {
                State::Failed => Some(format!(
                    "{}: connector `{}` is not loaded",
                    s.failure.as_deref().unwrap_or(BUNDLE_INVALID),
                    s.name
                )),
                State::DeadLetter => Some(format!(
                    "{DEAD_LETTER}: connector `{}` is in the dead-letter state",
                    s.name
                )),
                _ => None,
            })
            .collect()
    }

    /// The definitions that run on a schedule and are enabled.
    #[must_use]
    pub fn scheduled(&self) -> Vec<ConnectorDef> {
        let errors = lock(&self.load_errors).clone();
        self.defs
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|d| {
                d.state == State::Enabled && d.schedule.is_some() && !errors.contains_key(&d.name)
            })
            .cloned()
            .collect()
    }

    /// The job arguments of one run.
    #[must_use]
    pub fn job_args(connector: &str, operation: &str, input: &Value) -> Value {
        json!({ "connector": connector, "operation": operation, "input": input })
    }

    fn gate(&self, name: &str) -> Arc<Semaphore> {
        let mut gates = lock(&self.gates);
        Arc::clone(gates.entry(name.to_owned()).or_insert_with(|| {
            Arc::new(Semaphore::new(
                self.parts.max_concurrent_runs.max(1) as usize
            ))
        }))
    }

    async fn record(
        &self,
        name: &str,
        operation: &str,
        outcome: Outcome,
        started: i64,
        code: Option<&str>,
        attempt: u32,
    ) {
        metrics::counter!("connector_runs_total", "outcome" => outcome.as_str()).increment(1);
        let now = self.now_micros();
        let record = RunRecord {
            site: self.parts.site.clone(),
            connector: name.to_owned(),
            operation: operation.to_owned(),
            outcome,
            duration_ms: u64::try_from((now - started).max(0) / 1_000).unwrap_or(0),
            code: code.map(str::to_owned),
            attempt,
            at_micros: now,
        };
        // A log that cannot be written must not turn a good run into a failed one.
        if self.parts.runs.append(record).await.is_err() {
            tracing::warn!(
                connector = name,
                "the connector run log could not be written"
            );
        }
    }

    /// Runs one job: the handler of `platform:connector_run`.
    pub async fn run_job(&self, args: &Value, attempt: u32, last_attempt: bool) -> HandlerResult {
        let (Some(name), Some(operation)) = (
            args.get("connector").and_then(Value::as_str),
            args.get("operation").and_then(Value::as_str),
        ) else {
            return HandlerResult::Fail(BAD_JOB.to_owned());
        };
        let input = args.get("input").cloned().unwrap_or(Value::Null);
        let Some(id) = ConnectorId::new(name.to_owned()) else {
            return HandlerResult::Fail(BAD_JOB.to_owned());
        };
        let started = self.now_micros();
        let runnable = self
            .status()
            .into_iter()
            .find(|s| s.name == name)
            .is_some_and(|s| s.state == State::Enabled);
        if !runnable {
            self.record(
                name,
                operation,
                Outcome::Failed,
                started,
                Some(DISABLED),
                attempt,
            )
            .await;
            return HandlerResult::Fail(DISABLED.to_owned());
        }
        let Ok(permit) = self.gate(name).try_acquire_owned() else {
            self.record(
                name,
                operation,
                Outcome::Retry,
                started,
                Some(BUSY),
                attempt,
            )
            .await;
            return HandlerResult::Retry(BUSY.to_owned());
        };
        let limits = self.parts.limits.for_app(name);
        let deadline = started
            .saturating_add(i64::try_from(limits.wall_clock.as_micros()).unwrap_or(i64::MAX));
        let call = ConnectorCall {
            connector: id,
            operation: operation.to_owned(),
            input,
            tenant: self.parts.site.clone(),
            actor: Some(format!("{ACTOR_PREFIX}{name}")),
            now_micros: started,
            deadline_micros: deadline,
        };
        let outer = limits.wall_clock + Duration::from_secs(5);
        let result = tokio::time::timeout(outer, self.parts.host.invoke(call)).await;
        drop(permit);
        let failure = match result {
            Ok(Ok(_)) => {
                lock(&self.last_ok).insert(name.to_owned(), self.now_micros());
                self.record(name, operation, Outcome::Ok, started, None, attempt)
                    .await;
                return HandlerResult::Ok;
            }
            Ok(Err(error)) => (error.code(), error.class()),
            Err(_elapsed) => (TIMEOUT, FailureClass::Retryable),
        };
        let (code, class) = failure;
        match class {
            FailureClass::Permanent => {
                self.record(
                    name,
                    operation,
                    Outcome::Failed,
                    started,
                    Some(code),
                    attempt,
                )
                .await;
                HandlerResult::Fail(code.to_owned())
            }
            FailureClass::Retryable if !last_attempt => {
                self.record(
                    name,
                    operation,
                    Outcome::Retry,
                    started,
                    Some(code),
                    attempt,
                )
                .await;
                HandlerResult::Retry(code.to_owned())
            }
            FailureClass::Retryable => {
                self.record(
                    name,
                    operation,
                    Outcome::DeadLetter,
                    started,
                    Some(code),
                    attempt,
                )
                .await;
                // Best effort: the state change must not hide the failure.
                let _ = self
                    .parts
                    .directory
                    .set_state(&self.parts.site, name, State::DeadLetter)
                    .await;
                if let Some(def) = self
                    .defs
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_mut(name)
                {
                    def.state = State::DeadLetter;
                }
                self.parts.alerts.dead_letter(&self.parts.site, name, code);
                HandlerResult::Fail(DEAD_LETTER.to_owned())
            }
        }
    }
}
