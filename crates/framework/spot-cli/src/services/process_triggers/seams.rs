//! The seams of the trigger intake: what it reads of the site's metadata and secrets, where it
//! audits, and how a platform-read document becomes an event image and permission facts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_app_api::document::Document;
use spot_app_api::prelude::Value;
use spot_bpm_triggers::DocFactsSource;
use spot_permissions::DocFacts;
use spot_secrets::{KeyProvider, Sealed, SecretBox};
use time::format_description::well_known::Rfc3339;

use super::SealedSecret;
use crate::live::SiteLive;

/// Locks a mutex, recovering from a poisoned one (the guarded data is plain collections).
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A seam future.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the module reads of the site's live metadata.
pub trait Meta: Send + Sync {
    /// The doctypes that have a lifecycle with states, from the metadata served now; `None` when
    /// it cannot be read (every document start then denies).
    fn stateful(&self) -> Option<BTreeSet<String>>;
    /// The permission engine served now; `None` makes a document start for a person deny.
    fn engine(&self) -> Option<spot_permissions::PermissionEngine>;
}

/// [`Meta`] over the site's live hub.
pub struct LiveMeta(pub Arc<SiteLive>);

impl Meta for LiveMeta {
    fn stateful(&self) -> Option<BTreeSet<String>> {
        let served = self.0.served()?;
        Some(
            served
                .meta
                .doctypes()
                .filter(|d| d.is_posted)
                .map(|d| d.name.as_str().to_owned())
                .collect(),
        )
    }
    fn engine(&self) -> Option<spot_permissions::PermissionEngine> {
        self.0.served().map(|s| (*s.decision).clone())
    }
}

/// Where a webhook secret comes from, by reference.
pub trait SecretSource: Send + Sync {
    /// The secret bytes of `reference`, or `None` when there is none or it does not open.
    fn secret<'a>(&'a self, reference: &'a str) -> Fut<'a, Option<Vec<u8>>>;
}

/// Sealed secrets from the settings, opened with the site's key provider at call time only.
pub struct SealedSecrets<P: KeyProvider> {
    opener: SecretBox<P>,
    entries: BTreeMap<String, SealedSecret>,
}

impl<P: KeyProvider> SealedSecrets<P> {
    /// Secrets `entries` opened with `provider`.
    pub fn new(provider: P, entries: BTreeMap<String, SealedSecret>) -> Self {
        Self {
            opener: SecretBox::new(provider),
            entries,
        }
    }
}

impl<P: KeyProvider + 'static> SecretSource for SealedSecrets<P> {
    fn secret<'a>(&'a self, reference: &'a str) -> Fut<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let entry = self.entries.get(reference)?;
            let sealed = Sealed::from_str(&entry.sealed).ok()?;
            let opened = self.opener.open(&sealed, entry.aad.as_bytes()).await.ok()?;
            Some(opened.expose().as_bytes().to_vec())
        })
    }
}

/// One audited start or refusal. Never a value, a body, a signature or a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The site.
    pub site: String,
    /// `document`, `schedule` or `webhook`.
    pub source: &'static str,
    /// `started`, `existing` or `refused`.
    pub outcome: &'static str,
    /// The rule that decided (`ok`, `recursion`, `rate_limited`, `bad_signature`, ...).
    pub rule: String,
    /// The process, empty when none applies (a webhook refusal).
    pub process: String,
    /// The trigger node, empty when none applies.
    pub node: String,
    /// The kind of actor: `user`, `platform` or `caller`.
    pub who: &'static str,
}

/// Where the audit entries go.
pub trait Audit: Send + Sync {
    /// Records one entry.
    fn record(&self, entry: AuditEntry);
}

/// The default sink: the `spot::audit::process_triggers` log target.
pub struct TracingAudit;

impl Audit for TracingAudit {
    fn record(&self, e: AuditEntry) {
        tracing::info!(
            target: "spot::audit::process_triggers",
            site = %e.site, source = e.source, outcome = e.outcome, rule = %e.rule,
            process = %e.process, node = %e.node, who = e.who,
            "process trigger"
        );
    }
}

/// The platform-read image of a document: scalar fields as JSON (decimals as text, as the native
/// API encodes them) and the record id as `name`. State is not in the image: the loader carries it.
pub fn doc_image(doc: &Document) -> Json {
    let mut out = serde_json::Map::new();
    for (name, value) in doc.fields() {
        let json = match value {
            Value::Null => Json::Null,
            Value::Bool(b) => json!(b),
            Value::Int(i) => json!(i),
            Value::Decimal(d) => json!(d.to_string()),
            Value::Text(t) => json!(t),
            Value::Date(d) => json!(d.to_string()),
            Value::Id(id) => json!(id.to_string()),
            Value::DateTime(at) => match at.format(&Rfc3339) {
                Ok(text) => json!(text),
                Err(_) => continue,
            },
            Value::Json(text) => match serde_json::from_str::<Json>(text) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            },
            _ => continue,
        };
        out.insert(name.as_str().to_owned(), json);
    }
    out.insert("name".to_owned(), json!(doc.id().to_string()));
    Json::Object(out)
}

/// The permission facts of a platform-read document (scalar values, state, creator).
pub fn doc_facts(doc: &Document) -> DocFacts {
    let values: BTreeMap<String, Value> = doc
        .fields()
        .iter()
        .filter(|(_, v)| {
            matches!(
                v,
                Value::Null
                    | Value::Bool(_)
                    | Value::Int(_)
                    | Value::Decimal(_)
                    | Value::Text(_)
                    | Value::Date(_)
                    | Value::Time(_)
                    | Value::DateTime(_)
                    | Value::Id(_)
            )
        })
        .map(|(n, v)| (n.as_str().to_owned(), v.clone()))
        .collect();
    let created_by = match values.get("created_by") {
        Some(Value::Id(id)) => Some(id.to_string()),
        Some(Value::Text(t)) => Some(t.clone()),
        _ => None,
    };
    DocFacts {
        doctype: doc.doctype().as_str().to_owned(),
        id: Some(doc.id().to_string()),
        created_by,
        state: doc.state().map(|s| s.as_str().to_owned()),
        values,
    }
}

/// The facts the platform read for one event, served to the intake's loader hook.
pub(super) struct Preloaded(pub(super) Mutex<HashMap<String, DocFacts>>);

impl DocFactsSource for Preloaded {
    fn facts(&self, event: &spot_webhooks::CommittedEvent) -> Option<DocFacts> {
        lock(&self.0).get(&event.event_id).cloned()
    }
}

/// No secrets service: every webhook is disabled.
pub struct NoSecrets;

impl SecretSource for NoSecrets {
    fn secret<'a>(&'a self, _reference: &'a str) -> Fut<'a, Option<Vec<u8>>> {
        Box::pin(async { None })
    }
}
