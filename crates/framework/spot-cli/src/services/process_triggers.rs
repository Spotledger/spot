//! The process trigger intake of one site (WP-BPM-03.T4; ADR-49, ADR-080): what starts a Case.
//!
//! `spot-bpm-triggers` matches a source event against the published process versions and starts
//! exactly one Case per `(process, trigger node, source event)`; this module feeds it and hosts
//! its three intake paths over the engine host (`crate::services::process`):
//!
//! - **Document events.** The lease holder reads the site's outbox from a **durable position**
//!   (`spot_bpm_trigger_mark`, advanced only after a batch is taken in), turns each lifecycle
//!   event a trigger watches into a `SourceEvent::document` whose after-image is **read from the
//!   platform** (`SystemReader`), never taken from the thin event payload, and hands it to the
//!   intake. A redelivery, a second node or a restart derives the same start key, so the same
//!   Case. State completeness is enforced here: the stateful doctype set is rebuilt from the
//!   metadata served *now* for every event, the document loader supplies the state the image
//!   lacks, unreadable metadata denies every document start, and an unreadable document is
//!   retried a few times and then denied (reported, never silently dropped).
//! - **Schedules.** A tick every 15 s plans the due slots of each published schedule trigger with
//!   `spot_scheduler::policy::plan` (the trigger's own missed-run policy) from a durable high-water
//!   mark; the slot key is the dedup key, so two nodes ticking one slot start one Case.
//! - **Webhooks.** [`HookPort`] implements the public `WebhookPort` (`POST
//!   /api/v1/process/hook/{trigger}`, declared as the one open path of this module). The signature
//!   over the **raw body** is verified before anything is parsed, the secret is opened from the
//!   site's sealed secrets by reference, and every refusal (unknown or disabled trigger, missing
//!   secret, bad signature, stale timestamp, bad delivery id, non-object body) is the same
//!   `WebhookRejected` after the same work (the MAC is always computed, with a decoy key when no
//!   secret is usable). A replay is the same start key, so it starts nothing.
//!
//! # Rules the module keeps
//!
//! - **No recursion.** A Document node writes with the command id `<case>:<node>:<attempt>` (the
//!   engine's idempotency key), which the outbox event carries as `meta.command_id`. A trigger
//!   never starts on an event whose origin Case runs the same process version: the start is
//!   refused and audited (`recursion`).
//! - **Start rate.** At most `start_rate_per_minute` (default 100) new Cases per process per
//!   clock minute; a start over it is an audited refusal (`rate_limited`) and a health warning.
//! - **Principals.** A document start is judged as the event's actor (resolved through the site's
//!   IAM; a non-user actor is the platform and is not judged); schedule and webhook starts are the
//!   platform's. Nothing a caller sends ever selects the system principal.
//! - **Logs and audit.** Target `spot::audit::process_triggers` records who (actor kind), what
//!   (process, node) and which rule for every start and every refusal. Never a document value, a
//!   body, a signature, a secret or a delivery id.
//!
//! # Configuration
//!
//! `[services.process_triggers]`: `enabled` (true), `start_rate_per_minute` (100), `batch` (100),
//! `webhook_tolerance_secs` (300), `webhook_rate_per_minute` (600, the open path's limit),
//! `[services.process_triggers.webhooks.<trigger>] secret = "<ref>"` and
//! `[services.process_triggers.secrets.<ref>] sealed = ".." aad = ".."` (sealed under the site's
//! `[services.secrets]` key). The configuration names a secret, it never holds one in clear.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::Method;
use serde::Deserialize;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_core::SiteId;
use spot_document::engine::PrincipalSource;
use spot_http::api::Ports;
use spot_http::api::ports::webhook::WebhookPort;
use spot_http::api::router::routes::{self, Registration};
use tokio::sync::watch;

use crate::services::ports::{JobSubmit, OutboxLease, SystemReader};
use crate::services::process::ProcessHost;
use crate::services::{
    CheckFuture, Level, OpenPath, RateLimit, RunFuture, Service, SiteCtx, secrets::SecretsHost,
};
use crate::{Error, Result};

#[path = "process_triggers/host.rs"]
mod host;
#[path = "process_triggers/seams.rs"]
mod seams;

use self::seams::lock;

pub use host::{HookPort, Parts, Progress, StatsSnapshot, TriggerHost, case_id};
pub use seams::{
    Audit, AuditEntry, Fut, LiveMeta, Meta, NoSecrets, SealedSecrets, SecretSource, TracingAudit,
    doc_facts, doc_image,
};

/// Start order: after the process engine (1100) and the node-kind modules (1110 to 1115).
pub const ORDER: u32 = 1130;

/// The trigger intake as a port: what a publish or a retire calls so its triggers change at once
/// instead of on the next 15 s pass (WP-BPM-03.T5).
///
/// Provided by this module on every site where the intake runs; a module that reads it treats a
/// missing port (the intake is off on this site) as "nothing to reload", never as an error.
pub trait TriggerReload: Send + Sync {
    /// Reconciles the trigger index (document, schedule and webhook triggers) from the live
    /// published versions now. It is the code the 15 s pass runs, not a copy: a published set that
    /// did not change costs one read and changes nothing, and two calls never start a second
    /// schedule slot (the slot key is the dedup key).
    ///
    /// # Errors
    /// The definition store's failure. The trigger set in force before the call stays in force.
    fn reload_now(&self) -> Fut<'_, Result<usize>>;
}

impl TriggerReload for TriggerHost {
    fn reload_now(&self) -> Fut<'_, Result<usize>> {
        Box::pin(async move { self.reload().await })
    }
}

const CONSUMER: &str = "process_triggers";
const TICK: Duration = Duration::from_secs(15);
const HOOK_PATH: &str = "/api/v1/process/hook/{trigger}";
/// A document that cannot be read is retried this many passes, then the start is denied.
const READ_ATTEMPTS: u32 = 5;
/// The key a missing secret is replaced by, so every refusal does the same work.
const DECOY: &[u8] = b"spot-process-webhook-decoy-secret";

/// `[services.process_triggers]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// New Cases per process per clock minute (1 to 100000).
    pub start_rate_per_minute: u32,
    /// Outbox events per read (1 to 500).
    pub batch: u32,
    /// How far a webhook timestamp may be from now, seconds (30 to 3600).
    pub webhook_tolerance_secs: i64,
    /// The open path's per-site request limit per minute (1 to 100000).
    pub webhook_rate_per_minute: u32,
    /// Webhook triggers (by name, the trigger node's `path`) and the secret each is signed with.
    pub webhooks: BTreeMap<String, WebhookSetting>,
    /// Sealed secrets by reference.
    pub secrets: BTreeMap<String, SealedSecret>,
}

/// One webhook trigger's configuration: the name of its secret, never the secret.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSetting {
    /// The reference into `secrets`.
    pub secret: String,
}

/// One secret sealed under the site's key provider.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedSecret {
    /// The `spsec:v1:` sealed text.
    pub sealed: String,
    /// The associated data it was sealed under.
    pub aad: String,
}

impl std::fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealedSecret(..)")
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            start_rate_per_minute: 100,
            batch: 100,
            webhook_tolerance_secs: 300,
            webhook_rate_per_minute: 600,
            webhooks: BTreeMap::new(),
            secrets: BTreeMap::new(),
        }
    }
}

impl Settings {
    fn validate(&self) -> std::result::Result<(), String> {
        let bad = |key: &str| Err(format!("services.process_triggers.{key}: out of range"));
        if !(1..=100_000).contains(&self.start_rate_per_minute) {
            return bad("start_rate_per_minute");
        }
        if !(1..=500).contains(&self.batch) {
            return bad("batch");
        }
        if !(30..=3600).contains(&self.webhook_tolerance_secs) {
            return bad("webhook_tolerance_secs");
        }
        if !(1..=100_000).contains(&self.webhook_rate_per_minute) {
            return bad("webhook_rate_per_minute");
        }
        if self.webhooks.keys().any(|name| !trigger_name(name)) {
            return bad("webhooks");
        }
        Ok(())
    }
}

/// The grammar of a trigger name in the URL: `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn trigger_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let first = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A caller's delivery id: 1 to 128 of `[A-Za-z0-9._:-]`, never anything that could be a path, a
/// header or an expression.
fn delivery_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

struct TriggerService {
    host: Arc<TriggerHost>,
    outbox: Arc<dyn OutboxLease>,
}

impl Service for TriggerService {
    fn name(&self) -> &'static str {
        "process_triggers"
    }

    fn level(&self) -> Level {
        // A broken trigger intake degrades the processes it starts; documents still serve.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.host.reload().await.map_err(|e| e.to_string())?;
            let warnings = self.host.warnings();
            if warnings.is_empty() {
                Ok(())
            } else {
                Err(warnings.join("; "))
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if let Ok(lease) = self.outbox.acquire(CONSUMER).await
                    && self.lead(&mut stop, lease).await
                {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(TICK) => {}
                }
            }
        })
    }
}

impl TriggerService {
    /// Runs the intake as the lease holder; `true` means the node was stopped.
    async fn lead(
        &self,
        stop: &mut watch::Receiver<bool>,
        mut lease: crate::services::ports::ConsumerLease,
    ) -> bool {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.outbox.renew(&lease).await {
                        Ok(renewed) => lease = renewed,
                        Err(_) => break false,
                    }
                    if !matches!(self.outbox.still_held(&lease).await, Ok(true)) {
                        break false;
                    }
                    if let Err(e) = self.host.cycle(&*self.outbox).await {
                        tracing::warn!(site = %self.host.site(), %e, "the trigger intake pass failed");
                    }
                }
            }
        };
        let _ = self.outbox.release(&lease).await;
        stopped
    }
}

/// The ports of the sites built so far, so a reload build declares the same route and path.
fn built() -> &'static Mutex<HashMap<String, Arc<dyn WebhookPort>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<dyn WebhookPort>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// Registers the trigger intake: its service, the webhook port, the open path and the route.
/// Inert when `services.process_triggers.enabled = false` or the process engine is off.
///
/// # Errors
/// A setting that does not decode or is out of range; a table that cannot be prepared; a path or
/// port another module already declared.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_triggers")?;
    settings.validate().map_err(Error)?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let port: Arc<dyn WebhookPort> = if ctx.assembling() {
        let Some(process) = ctx.port::<ProcessHost>() else {
            tracing::info!(site = %site_name, "the process engine is off; no process triggers");
            return Ok(());
        };
        let site = SiteId::new(&site_name).map_err(|e| Error(e.to_string()))?;
        let pool = env.backend.pool();
        let definitions = Arc::new(PgDefinitionStore::new(pool.clone()).with_tenant(site.as_str()));
        definitions
            .prepare()
            .await
            .map_err(|e| Error(format!("the definition store: {}", e.code())))?;
        let secrets: Arc<dyn SecretSource> = match ctx.port::<SecretsHost>() {
            Some(host) => Arc::new(SealedSecrets::new(
                Arc::clone(host.keyring()),
                settings.secrets.clone(),
            )),
            None => Arc::new(NoSecrets),
        };
        let outbox = ctx.require_port::<dyn OutboxLease>()?;
        let host = TriggerHost::new(Parts {
            site,
            clock: Arc::clone(ctx.clock()),
            settings: settings.clone(),
            process,
            definitions,
            submit: ctx.require_port::<dyn JobSubmit>()?,
            reader: ctx.require_port::<dyn SystemReader>()?,
            principals: Arc::clone(&env.permission.principals) as Arc<dyn PrincipalSource>,
            meta: Arc::new(LiveMeta(Arc::clone(env.live))),
            secrets,
            audit: Arc::new(TracingAudit),
            pool,
        });
        host.prepare().await?;
        ctx.add(Arc::new(TriggerService {
            host: Arc::clone(&host),
            outbox,
        }));
        let reload: Arc<dyn TriggerReload> = Arc::clone(&host) as Arc<dyn TriggerReload>;
        ctx.provide::<dyn TriggerReload>(reload)?;
        let port: Arc<dyn WebhookPort> = Arc::new(HookPort::new(host));
        lock(built()).insert(site_name, Arc::clone(&port));
        ctx.provide::<dyn WebhookPort>(Arc::clone(&port))?;
        port
    } else {
        match lock(built()).get(&site_name).cloned() {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    // The one open path of this module: the host authenticates everything else.
    ctx.open_paths(&[OpenPath {
        method: Method::POST,
        pattern: HOOK_PATH,
        limit: RateLimit {
            per_minute: settings.webhook_rate_per_minute,
        },
    }])?;
    let mut ports = Ports::new();
    ports.provide::<dyn WebhookPort>(port);
    let registration = Registration {
        name: "webhook",
        serves: routes::webhook::SERVES,
        required_ports: routes::webhook::required_ports,
        routes: routes::webhook::routes,
        public: Some(routes::webhook::PUBLIC),
    };
    let mounted = routes::mount(&[registration], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the process webhook route was not mounted"
        );
    }
    let public: Vec<_> = mounted
        .public
        .into_iter()
        .map(|(_, router)| router)
        .collect();
    ctx.router_layer(move |base| {
        public
            .into_iter()
            .fold(base, |acc, router| acc.merge(router))
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_bpm_triggers::StartKey;

    #[test]
    fn settings_default_validate_and_refuse_unknown_keys() {
        assert!(Settings::default().validate().is_ok());
        assert_eq!(Settings::default().start_rate_per_minute, 100);
        assert!(toml::from_str::<Settings>("start_rate = 5").is_err());
        let bad = Settings {
            batch: 0,
            ..Settings::default()
        };
        assert!(bad.validate().is_err());
        let mut hooks = Settings::default();
        hooks
            .webhooks
            .insert("Bad Name".into(), WebhookSetting { secret: "s".into() });
        assert!(hooks.validate().is_err());
    }

    #[test]
    fn trigger_names_and_delivery_ids_follow_their_grammar() {
        assert!(trigger_name("intake-1"));
        assert!(!trigger_name("-intake"));
        assert!(!trigger_name("a/b"));
        assert!(!trigger_name(&"a".repeat(65)));
        assert!(delivery_id_ok("evt_1:abc.2-x"));
        assert!(!delivery_id_ok(""));
        assert!(!delivery_id_ok("a b"));
        assert!(!delivery_id_ok("../etc"));
        assert!(!delivery_id_ok(&"a".repeat(129)));
    }

    #[test]
    fn a_start_key_derives_one_stable_distinct_case_id() {
        let node = spot_bpm_model::NodeId::new("start").unwrap();
        let a = case_id(&StartKey::new("p", node.clone(), "e1")).unwrap();
        assert_eq!(a, case_id(&StartKey::new("p", node.clone(), "e1")).unwrap());
        assert_ne!(a, case_id(&StartKey::new("p", node.clone(), "e2")).unwrap());
        assert_ne!(a, case_id(&StartKey::new("pe", node, "1")).unwrap());
    }
}
