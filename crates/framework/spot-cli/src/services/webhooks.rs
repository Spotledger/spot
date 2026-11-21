//! Outbound webhooks hosted per site (WP-INT-01.T2). Operator guide: `docs/running-spot/webhooks.md`.
//!
//! Every committed event whose DocType and event name match an enabled `webhook` document
//! becomes one signed `POST` through the egress guard, delivered as a job on the site's worker.
//!
//! - **Intake** ([`Pump`]): one node holds the `webhooks` outbox lease, reads the outbox from a
//!   cursor kept in the site database ([`PgCursor`]), matches webhooks, evaluates each condition on
//!   the document image and submits one job per (event, webhook). The delivery id is derived from
//!   the webhook and the event ([`delivery_id`]) and is the job's idempotency key, so a replayed
//!   intake or a restart creates no second job. The cursor moves only after a page is submitted.
//! - **Image**: the document is read as the platform itself (`SystemReader`), then cut down to the
//!   fields with no protection group, classification or mask ([`image_of`]): the payload never
//!   holds a field a least-privileged reader could not see, and no protected value enters a job row.
//! - **Delivery**: `spot_webhooks::DeliveryHandler`, registered on `ctx.job_handlers()`, over
//!   [`EngineWebhooks`] (reads `webhook` documents), [`EngineDeliveryLog`] (one `webhook_delivery`
//!   document per attempt, no body, no URL, no message from the target) and [`SigningResolver`]
//!   (the signing key through the WP-INT-01.T5 port; without it nothing is sent).
//! - **Egress**: the `webhook` purpose only; private, loopback, link-local and metadata addresses
//!   are refused by the guard, which connects to the address it checked; redirects are not followed.
//!
//! # Level
//!
//! Optional. No secrets vault, no `webhooks` queue in `services.jobs.queues`, or an unreachable
//! cursor table degrades the site and runs nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::error::ErrorClass;
use spot_app_api::meta::DocTypeDef;
use spot_core::{Clock, SiteId};
use spot_document::engine::Engine;
use spot_egress::{
    AllowEntry, EgressConnector, EgressPolicy, Guard, HttpSender, InMemorySink, Purpose, Sender,
    TenantPolicy,
};
use spot_events_contract::Topic;
use spot_jobs::model::{InsertResult, Job, JobId, NewJob};
use spot_jobs::store::{ClaimedJob, JobFilter, JobStore};
use spot_postgres::PgBackend;
use spot_secrets::SecretString;
use spot_storage_contract::{
    Catalog, Expr, FeedPosition, Isolation, Name, OutboxRecord, Query, QueryExecutor, QueryPath,
    RecordId, Selection, StorageBackend, TextMatch, TextMode, Transaction, TxOptions, Value,
    Version,
};
use spot_webhooks::delivery::{DeliveryHandler, DeliveryLog, Egress, SecretResolver, SpotEgress};
use spot_webhooks::intake::idempotency_key;
use spot_webhooks::{
    AllFieldsReadable, BulkheadConfig, CircuitBreakerConfig, DeliveryId, DeliveryJob,
    DeliveryStatus, Error as WhError, EventType, Header, PayloadMode, RateLimitConfig, Resilience,
    SealedSecret, Webhook, WebhookConfig, WebhookDelivery, WebhookId, WebhookState, WebhookStore,
    evaluate_condition,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;
use uuid::Uuid;

use crate::live::SiteLive;
use crate::services::ports::{
    ConsumerLease, JobSpec, JobSubmit, OutboxLease, PortError, SystemReader,
};
use crate::services::process_integration::Wiring;
use crate::services::secrets::SecretsHost;
use crate::services::webhook_secret::{SigningKeyError, WebhookSigningKeys};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "webhooks/engine.rs"]
mod engine;
#[path = "webhooks/intake.rs"]
mod intake;
#[path = "webhooks/jobs.rs"]
mod jobs;
#[cfg(test)]
#[path = "webhooks/tests.rs"]
mod tests;

pub use engine::{EngineAccess, EngineDeliveryLog, EngineWebhooks, SigningResolver};
use intake::WebhooksService;
pub use intake::{CursorStore, EngineImages, Image, Images, MemoryCursor, PgCursor, Pump};
pub use jobs::SubmitOnlyJobs;

/// Start order: after the signing-secret module (1030), whose port this module takes.
pub const ORDER: u32 = 1060;

const WEBHOOK: &str = "webhook";
const DELIVERY: &str = "webhook_delivery";
/// The consumer lease of the intake (`outbox:<site>:webhooks`).
const CONSUMER: &str = "webhooks";
/// The marker the store puts in `SealedSecret::sealed`: the webhook's name, never a secret.
const SIGNING_MARK: &str = "webhook-signing:";
/// The namespace of derived delivery ids.
const ID_NAMESPACE: Uuid = Uuid::from_u128(0x5b0a_7e4c_9d21_4f3e_8a60_c1d2_e3f4_0617);
/// The most rows a lookup returns.
const MAX_ROWS: u32 = 1000;

/// `[services.webhooks]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// The retry ceiling: no webhook may ask for more attempts (the first included).
    pub max_attempts: u32,
    /// The most bytes a payload may take.
    pub max_payload_bytes: usize,
    /// The most deliveries in flight to one endpoint.
    pub per_endpoint_concurrency: u32,
    /// The request timeout, seconds.
    pub timeout_secs: u64,
    /// The most response bytes read (the body is discarded, never logged).
    pub max_response_bytes: u64,
    /// How often the intake reads the outbox, milliseconds.
    pub poll_ms: u64,
    /// Outbox events per read.
    pub batch_size: u32,
    /// When non-empty, the only hosts (`host` or `*.suffix`) a webhook may reach.
    pub allowed_hosts: Vec<String>,
    /// Deliver events committed before this module first ran (default: only later events).
    pub replay_history: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 5,
            max_payload_bytes: 256 * 1024,
            per_endpoint_concurrency: 4,
            timeout_secs: 15,
            max_response_bytes: 64 * 1024,
            poll_ms: 1000,
            batch_size: 100,
            allowed_hosts: Vec::new(),
            replay_history: false,
        }
    }
}

impl Settings {
    /// The first setting out of range, by key.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        let ok = [
            ("max_attempts", (1..=20).contains(&self.max_attempts)),
            (
                "max_payload_bytes",
                (1..=1_048_576).contains(&self.max_payload_bytes),
            ),
            (
                "per_endpoint_concurrency",
                (1..=64).contains(&self.per_endpoint_concurrency),
            ),
            ("timeout_secs", (1..=120).contains(&self.timeout_secs)),
            (
                "max_response_bytes",
                (1..=1_048_576).contains(&self.max_response_bytes),
            ),
            ("poll_ms", (100..=60_000).contains(&self.poll_ms)),
            ("batch_size", (1..=500).contains(&self.batch_size)),
        ];
        match ok.iter().find(|(_, good)| !good) {
            Some((key, _)) => Err(key),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// The delivery id of one event and one webhook: the same on every replay and every retry.
pub fn delivery_id(webhook: WebhookId, event_id: &str) -> DeliveryId {
    DeliveryId(Uuid::new_v5(
        &ID_NAMESPACE,
        format!("{webhook}:{event_id}").as_bytes(),
    ))
}

/// The webhook id of a `webhook` record.
pub fn webhook_id_of(id: RecordId) -> WebhookId {
    WebhookId(Uuid::from_bytes(*id.as_bytes()))
}

fn record_id_of(id: WebhookId) -> Option<RecordId> {
    RecordId::from_bytes(*id.0.as_bytes()).ok()
}

fn value_json(value: &Value) -> Option<Json> {
    Some(match value {
        Value::Null => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::DateTime(t) => Json::String(t.format(&Rfc3339).ok()?),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Json(text) => serde_json::from_str(text).ok()?,
        _ => return None,
    })
}

/// The document as a payload image: only fields with no protection group, no classification and
/// no mask. A field the type cannot carry as JSON is left out. Fail closed: when in doubt, absent.
pub fn image_of(def: &DocTypeDef, doc: &Document) -> Json {
    let mut out = serde_json::Map::new();
    out.insert("id".to_owned(), Json::String(doc.id().to_string()));
    for field in &def.fields {
        if !field.security.is_empty() {
            continue;
        }
        if let Some(json) = value_json(doc.get(field.name.as_str())) {
            out.insert(field.name.as_str().to_owned(), json);
        }
    }
    Json::Object(out)
}

/// Why a `webhook` document could not become a [`Webhook`]. Never a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unmappable(pub &'static str);

fn text(doc: &Document, field: &'static str) -> std::result::Result<String, Unmappable> {
    match doc.get(field) {
        Value::Text(t) => Ok(t.clone()),
        _ => Err(Unmappable(field)),
    }
}

fn json_field(doc: &Document, field: &'static str) -> Json {
    match doc.get(field) {
        Value::Json(t) => serde_json::from_str(t).unwrap_or(Json::Null),
        _ => Json::Null,
    }
}

/// A `webhook` document as the delivery crate's record, capped by the operator's ceilings.
/// `secret.sealed` holds the signing marker with the webhook's name, never the secret.
pub fn webhook_from_document(
    doc: &Document,
    settings: &Settings,
) -> std::result::Result<Webhook, Unmappable> {
    let name = text(doc, "name")?;
    let state = match text(doc, "state")?.as_str() {
        "active" => WebhookState::Active,
        "paused" => WebhookState::Paused,
        "disabled" => WebhookState::Disabled,
        "dead_letter" => WebhookState::DeadLetter,
        _ => return Err(Unmappable("state")),
    };
    let payload_mode = match text(doc, "payload_mode")?.as_str() {
        "ids" => PayloadMode::Ids,
        "fields" => PayloadMode::Fields,
        "full" => PayloadMode::Full,
        _ => return Err(Unmappable("payload_mode")),
    };
    let events = json_field(doc, "events")
        .as_array()
        .ok_or(Unmappable("events"))?
        .iter()
        .filter_map(|e| serde_json::from_value::<EventType>(e.clone()).ok())
        .collect::<Vec<_>>();
    let allowlist = json_field(doc, "field_allowlist")
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let headers = json_field(doc, "headers")
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|h| serde_json::from_value::<Header>(h.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let int = |field: &'static str, default: i64| match doc.get(field) {
        Value::Int(i) => *i,
        _ => default,
    };
    let at = |field: &'static str| match doc.get(field) {
        Value::DateTime(t) => *t,
        _ => OffsetDateTime::UNIX_EPOCH,
    };
    Ok(Webhook {
        id: webhook_id_of(doc.id()),
        name: name.clone(),
        enabled: matches!(doc.get("enabled"), Value::Bool(true)),
        state,
        url: text(doc, "url")?,
        doctype: text(doc, "doctype")?,
        events,
        condition: match doc.get("condition") {
            Value::Text(t) if !t.trim().is_empty() => Some(t.clone()),
            _ => None,
        },
        payload_mode,
        field_allowlist: allowlist,
        secret: SealedSecret {
            sealed: format!("{SIGNING_MARK}{name}"),
            previous: None,
        },
        headers,
        timeout_secs: u64::try_from(int("timeout_secs", 30))
            .unwrap_or(30)
            .clamp(1, settings.timeout_secs),
        max_attempts: u32::try_from(int("max_attempts", 5))
            .unwrap_or(5)
            .clamp(1, settings.max_attempts),
        created_at: at("created_at"),
        updated_at: at("updated_at"),
        last_delivery: serde_json::from_value(json_field(doc, "last_delivery")).ok(),
        state_reason: match doc.get("state_reason") {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        },
    })
}

fn clean_class(class: &str) -> String {
    class
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
        .take(64)
        .collect()
}

fn state_text(d: &WebhookDelivery) -> &'static str {
    match d.status {
        DeliveryStatus::Delivered => "delivered",
        DeliveryStatus::DeadLetter | DeliveryStatus::Failed | DeliveryStatus::Blocked => {
            "dead_letter"
        }
        DeliveryStatus::Pending if d.attempts > 0 && d.last_error_class.is_some() => "retrying",
        DeliveryStatus::Pending => "pending",
    }
}

// ---------------------------------------------------------------------------------------------
// Egress and registration
// ---------------------------------------------------------------------------------------------

/// The webhook egress of one site: the `webhook` purpose, the guard that connects to the address
/// it checked, no redirects, and (when `allowed_hosts` is set) only the listed hosts.
pub fn build_egress(
    site: &str,
    settings: &Settings,
    wiring: Wiring,
    clock: Arc<dyn Clock>,
) -> Arc<dyn Egress> {
    let mut policy: EgressPolicy = wiring.policy;
    if !settings.allowed_hosts.is_empty() {
        policy.default = "deny".to_owned();
        let allow = settings
            .allowed_hosts
            .iter()
            .map(|host| AllowEntry {
                scheme: None,
                host: host.clone(),
                ports: None,
                purpose: Some(Purpose::Webhook.as_str().to_owned()),
                allow_private: None,
            })
            .collect();
        policy.tenants.insert(
            site.to_owned(),
            TenantPolicy {
                allow,
                ..TenantPolicy::default()
            },
        );
    }
    let guard = Arc::new(Guard::new(
        policy,
        wiring.resolver,
        Arc::new(InMemorySink::new()),
        clock,
    ));
    let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), site.to_owned(), Purpose::Webhook),
        Duration::from_secs(settings.timeout_secs),
    ));
    Arc::new(SpotEgress::new(guard, sender, settings.max_response_bytes))
}

/// The handler's configuration from the operator's settings.
pub fn handler_config(settings: &Settings) -> WebhookConfig {
    WebhookConfig {
        default_timeout_secs: settings.timeout_secs,
        default_max_attempts: settings.max_attempts,
        max_payload_size_bytes: settings.max_payload_bytes,
        bulkhead: BulkheadConfig {
            max_concurrent: settings.per_endpoint_concurrency,
        },
        ..WebhookConfig::default()
    }
}

/// Joins the site: registers the delivery handler and the intake service.
///
/// # Errors
///
/// A setting out of range, or a port a module this one depends on did not provide.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("webhooks")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.webhooks.{key}: out of range")))?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let (site, access, pool, queue_missing) = {
        let env = *ctx.env()?;
        let site = SiteId::new(&env.site.site).map_err(crate::error)?;
        let access = EngineAccess {
            site: site.clone(),
            live: Arc::clone(env.live),
            clock: Arc::clone(ctx.clock()),
            catalog: env.permission.catalog.clone(),
        };
        let missing = !env
            .loaded
            .common
            .services
            .jobs
            .queues
            .iter()
            .any(|q| q == spot_webhooks::WEBHOOK_QUEUE);
        (site, access, env.backend.pool(), missing)
    };
    let submit = ctx.require_port::<dyn JobSubmit>()?;
    let outbox = ctx.require_port::<dyn OutboxLease>()?;
    let reader = ctx.require_port::<dyn SystemReader>()?;
    let keys = ctx.require_port::<dyn WebhookSigningKeys>()?;
    let has_vault = ctx.port::<SecretsHost>().is_some();
    let problem = if !has_vault {
        Some("no secrets vault ([services.secrets] is absent): webhooks cannot be signed, so none is sent".to_owned())
    } else if queue_missing {
        Some(format!(
            "`{}` is not in services.jobs.queues: add it so the worker claims webhook deliveries",
            spot_webhooks::WEBHOOK_QUEUE
        ))
    } else {
        None
    };
    let clock = Arc::clone(ctx.clock());
    let store: Arc<dyn WebhookStore> = Arc::new(EngineWebhooks {
        access: access.clone(),
        settings: settings.clone(),
    });
    let log: Arc<dyn DeliveryLog> = Arc::new(EngineDeliveryLog {
        access: access.clone(),
    });
    let resilience = Resilience::new(
        Arc::clone(&clock),
        CircuitBreakerConfig::default(),
        BulkheadConfig {
            max_concurrent: settings.per_endpoint_concurrency,
        },
        RateLimitConfig::default(),
    );
    let handler = DeliveryHandler::new(
        Arc::clone(&store),
        Arc::new(SubmitOnlyJobs(Arc::clone(&submit))),
        log,
        build_egress(
            site.as_str(),
            &settings,
            Wiring::production(),
            Arc::clone(&clock),
        ),
        Arc::new(SigningResolver::new(site.as_str(), keys)),
        Arc::new(AllFieldsReadable),
        resilience,
        Arc::clone(&clock),
        site.as_str().to_owned(),
        site.clone(),
        handler_config(&settings),
    )
    .map_err(crate::error)?;
    Arc::new(handler).register(ctx.job_handlers());
    // The side-effect hold (WP-MIG-08.T5): a held site delivers nothing. The queue gate keeps the
    // jobs queued; this guard covers a delivery claimed just before the hold began, before the
    // egress call. Without the hold service the handler behaves as before.
    if let Some(hold) = ctx.port::<spot_effect_hold::HoldService>() {
        crate::services::jobs::guard_job_handler(
            ctx.job_handlers(),
            spot_webhooks::WEBHOOK_JOB_NAME,
            spot_effect_hold::EffectClass::Webhook,
            hold.gate(),
        );
    }
    let cursor = PgCursor::new(pool, site.as_str());
    cursor
        .prepare()
        .await
        .map_err(|e| Error(format!("services.webhooks: {e}")))?;
    let max_args = crate::services::ports::MAX_ARGS_BYTES - 1024;
    ctx.add(Arc::new(WebhooksService {
        site: site.as_str().to_owned(),
        outbox,
        pump: Pump {
            store,
            images: Arc::new(EngineImages { reader, access }),
            submit,
            clock,
            max_args,
        },
        cursor: Arc::new(cursor),
        settings,
        problem,
    }));
    Ok(())
}
