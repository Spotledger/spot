//! The six conformance invariants, registered on T1's registry.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_events_contract::lifecycle::Operation;
use spot_events_contract::registry::{self, Class, Publishable, Retention};
use spot_events_contract::{PayloadDocument, identity::FORBIDDEN_KEY_PARTS};
use spot_l7_invariants::invariant::{
    BoxFuture, CheckContext, Invariant, InvariantRegistry, Violation,
};
use spot_storage_contract::{OutboxRecord, StorageResult};

use crate::classify::{self, Family, MUST_HAVE_SCHEMA_OWNERS};
use crate::coverage::ExpectedTopic;
use crate::secrets;

/// Id of [`Registered`].
pub const REGISTERED: &str = "events.registered";
/// Id of [`EventEnvelope`].
pub const ENVELOPE: &str = "events.envelope";
/// Id of [`PayloadSchema`].
pub const SCHEMA: &str = "events.payload_schema";
/// Id of [`EventClass`].
pub const CLASS: &str = "events.class";
/// Id of [`NoSecretValues`].
pub const SECRETS: &str = "events.no_secret_values";
/// Id of [`EventCoverage`].
pub const COVERAGE: &str = "events.coverage";

/// The journal's `doc_state` image holds the whole record on purpose (WP-JRN-02.T2): the secret
/// and key checks leave it to the journal's own protect transform, as the L7 engine invariants
/// do.
const IMAGE_TOPIC: &str = "doc_state";

fn topic_of(rec: &OutboxRecord) -> &str {
    rec.event.topic.as_str()
}

macro_rules! invariant {
    ($ty:ty, $id:expr, $desc:expr) => {
        impl Invariant for $ty {
            fn id(&self) -> &str {
                $id
            }
            fn describe(&self) -> &str {
                $desc
            }
            fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
                Box::pin(async move {
                    match self.run(ctx).await {
                        Ok(v) => v,
                        Err(e) => vec![Violation::read_failed($id, "outbox", &e)],
                    }
                })
            }
        }
    };
}

/// `events.registered`.
#[derive(Default)]
pub struct Registered {
    warnings: AtomicUsize,
}

impl Registered {
    /// App topics outside the registry seen by the last run (a warning, not a failure).
    pub fn app_topics_outside_the_registry(&self) -> usize {
        self.warnings.load(Ordering::SeqCst)
    }

    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let mut out = Vec::new();
        let mut warnings = 0;
        for rec in ctx.outbox().await? {
            match registry::check_publishable(topic_of(&rec)) {
                Ok(Publishable::Registered(_)) => {}
                Ok(Publishable::Warn) => warnings += 1,
                Err(_) => out.push(Violation::new(
                    REGISTERED,
                    "outbox",
                    rec.event.id,
                    format!("unregistered_platform_topic topic={}", topic_of(&rec)),
                )),
            }
        }
        self.warnings.store(warnings, Ordering::SeqCst);
        Ok(out)
    }
}

invariant!(
    Registered,
    REGISTERED,
    "Every topic in the outbox that is a platform topic is registered in the events contract \
     (a retired one too); an app topic outside the registry is counted, not failed."
);

/// `events.envelope`.
#[derive(Default)]
pub struct EventEnvelope;

impl EventEnvelope {
    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let mut out = Vec::new();
        for rec in ctx.outbox().await? {
            let topic = topic_of(&rec).to_owned();
            if let Err(e) = PayloadDocument::from_outbox_event(&rec.event) {
                let kind = match e {
                    spot_events_contract::PayloadError::Malformed(_) => "malformed",
                    spot_events_contract::PayloadError::DataNotObject => "data_not_object",
                    spot_events_contract::PayloadError::InvalidTraceId => "invalid_trace_id",
                };
                out.push(Violation::new(
                    ENVELOPE,
                    "outbox",
                    rec.event.id,
                    format!("invalid_envelope topic={topic} kind={kind}"),
                ));
                continue;
            }
            // A lifecycle event belongs to a document write: it carries the version it wrote.
            let lifecycle = Operation::ALL.iter().any(|o| o.topic_name() == topic);
            if lifecycle && rec.event.subject_version.is_none() {
                out.push(Violation::new(
                    ENVELOPE,
                    "outbox",
                    rec.event.id,
                    format!("missing_subject_version topic={topic}"),
                ));
            }
        }
        Ok(out)
    }
}

invariant!(
    EventEnvelope,
    ENVELOPE,
    "Every event's payload is a `{meta, data}` document whose own rules hold (data is an object, \
     the trace id is well formed), and every lifecycle event carries the version it wrote."
);

/// `events.payload_schema`.
#[derive(Default)]
pub struct PayloadSchema;

impl PayloadSchema {
    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let mut out = Vec::new();
        for rec in ctx.outbox().await? {
            let topic = topic_of(&rec).to_owned();
            // An unregistered topic is `events.registered`'s finding, not this one's.
            let Some(entry) = registry::lookup(&topic) else {
                continue;
            };
            let Ok(doc) = PayloadDocument::from_outbox_event(&rec.event) else {
                continue;
            };
            match classify::validate(&topic, &doc.data) {
                Ok(true) => {}
                Ok(false) => {
                    if MUST_HAVE_SCHEMA_OWNERS.contains(&entry.owner) {
                        out.push(Violation::new(
                            SCHEMA,
                            "outbox",
                            rec.event.id,
                            format!("no_schema topic={topic} owner={}", entry.owner),
                        ));
                    }
                }
                Err(paths) => out.push(Violation::new(
                    SCHEMA,
                    "outbox",
                    rec.event.id,
                    format!(
                        "schema_mismatch topic={topic} family={} paths={}",
                        classify::family_of(&topic).name(),
                        paths.join(",")
                    ),
                )),
            }
        }
        Ok(out)
    }
}

invariant!(
    PayloadSchema,
    SCHEMA,
    "Every event of a registered topic validates strictly against the payload schema of its \
     family (lifecycle, state, system, system_platform, journal). A topic with no schema anywhere \
     fails only when the registry says `spot-document`, `spot-jobs` or `spot-doc-audit` emits it: \
     no emitter without a schema. Mismatches name the topic and the JSON path, never a value."
);

/// `events.class`.
#[derive(Default)]
pub struct EventClass;

impl EventClass {
    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let mut out = Vec::new();
        let records = ctx.outbox().await?;
        let lifecycle_subjects: BTreeSet<_> = records
            .iter()
            .filter(|r| Operation::ALL.iter().any(|o| o.topic_name() == topic_of(r)))
            .map(|r| r.event.subject.id)
            .collect();
        for rec in &records {
            let topic = topic_of(rec);
            let Some(entry) = registry::lookup(topic) else {
                continue;
            };
            // An audit topic is kept for the audit retention, and only an audit topic is.
            let audit_retention = entry.retention == Retention::Audit;
            if (entry.class == Class::Audit) != audit_retention {
                out.push(Violation::new(
                    CLASS,
                    "outbox",
                    rec.event.id,
                    format!("class_retention_mismatch topic={topic}"),
                ));
            }
            // Coalescing is a declaration of Domain topics only (ADR-066).
            if entry.coalescing == registry::Coalescing::LatestPerSubject
                && entry.class != Class::Domain
            {
                out.push(Violation::new(
                    CLASS,
                    "outbox",
                    rec.event.id,
                    format!("coalescing_on_a_non_domain_topic topic={topic}"),
                ));
            }
            // The document audit step writes in the transaction that wrote the document, so a
            // document audit event always has the document's lifecycle event beside it.
            if topic == "sys_doc_audit" && !lifecycle_subjects.contains(&rec.event.subject.id) {
                out.push(Violation::new(
                    CLASS,
                    "outbox",
                    rec.event.id,
                    "audit_event_without_a_document_write".to_owned(),
                ));
            }
        }
        Ok(out)
    }
}

invariant!(
    EventClass,
    CLASS,
    "The registry's class and retention agree for every event seen (audit class if and only if \
     audit retention; coalescing only on Domain topics), and a document audit event has a \
     lifecycle event for the same document in the same outbox."
);

/// `events.no_secret_values`.
pub struct NoSecretValues {
    secrets: Vec<String>,
}

impl NoSecretValues {
    /// The invariant over the secret values the workload planted (`EngineRun::secrets`).
    pub fn new(secrets: Vec<String>) -> Self {
        Self { secrets }
    }

    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let mut out = Vec::new();
        for rec in ctx.outbox().await? {
            let topic = topic_of(&rec);
            if topic == IMAGE_TOPIC {
                continue;
            }
            for secret in &self.secrets {
                if let Some(form) = secrets::find(&rec.event.payload, secret) {
                    out.push(Violation::new(
                        SECRETS,
                        "outbox",
                        rec.event.id,
                        format!("secret_value topic={topic} form={form}"),
                    ));
                }
            }
            if let Ok(doc) = PayloadDocument::from_outbox_event(&rec.event) {
                let mut keys = Vec::new();
                forbidden_keys(&doc.data, "", &mut keys);
                for path in keys {
                    out.push(Violation::new(
                        SECRETS,
                        "outbox",
                        rec.event.id,
                        format!("forbidden_key topic={topic} path={path}"),
                    ));
                }
            }
        }
        Ok(out)
    }
}

fn forbidden_keys(value: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map {
                let here = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let lower = key.to_lowercase();
                if FORBIDDEN_KEY_PARTS.iter().any(|p| lower.contains(p)) {
                    out.push(here.clone());
                }
                forbidden_keys(inner, &here, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, inner) in items.iter().enumerate() {
                forbidden_keys(inner, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

invariant!(
    NoSecretValues,
    SECRETS,
    "No payload (the `doc_state` image aside, which the journal protects on the way out) \
     contains the exact value of a secret the workload wrote, nor its base64 or hex form, and \
     no payload key contains a forbidden part from the events contract's key list."
);

/// `events.coverage`.
pub struct EventCoverage {
    expected: Vec<ExpectedTopic>,
}

impl EventCoverage {
    /// The invariant over the topics a run must produce.
    pub fn new(expected: Vec<ExpectedTopic>) -> Self {
        Self { expected }
    }

    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let seen: BTreeSet<String> = ctx
            .outbox()
            .await?
            .iter()
            .map(|r| topic_of(r).to_owned())
            .collect();
        Ok(self
            .expected
            .iter()
            .filter(|e| !seen.contains(e.topic))
            .map(|e| {
                Violation::new(
                    COVERAGE,
                    "outbox",
                    e.topic,
                    format!("topic_never_emitted reason={}", e.reason),
                )
            })
            .collect())
    }
}

invariant!(
    EventCoverage,
    COVERAGE,
    "Every topic the table of expected topics lists was emitted at least once: a silently dead \
     emitter is a failure that names the topic."
);

/// The six invariants on one registry.
pub fn conformance_registry(
    secrets: Vec<String>,
    expected: Vec<ExpectedTopic>,
) -> InvariantRegistry {
    let mut registry = InvariantRegistry::new();
    let items: Vec<Box<dyn Invariant>> = vec![
        Box::new(Registered::default()),
        Box::new(EventEnvelope),
        Box::new(PayloadSchema),
        Box::new(EventClass),
        Box::new(NoSecretValues::new(secrets)),
        Box::new(EventCoverage::new(expected)),
    ];
    for item in items {
        // The ids are distinct; the unit tests prove it.
        let _ = registry.register(item);
    }
    registry
}

/// The six invariants and, when a [`RowSource`](crate::rows::RowSource) is given, the seventh:
/// every audit and job-enqueued event has its row, once (`events.rows_match`).
pub fn conformance_registry_with_rows(
    secrets: Vec<String>,
    expected: Vec<ExpectedTopic>,
    rows: std::sync::Arc<dyn crate::rows::RowSource>,
) -> InvariantRegistry {
    let mut registry = conformance_registry(secrets, expected);
    // The id is distinct from the six; the unit test proves it.
    let _ = registry.register(Box::new(crate::rows::RowsMatch::new(rows)));
    registry
}

/// The families that carry schemas, for the table.
pub fn families() -> [Family; 5] {
    [
        Family::Lifecycle,
        Family::State,
        Family::System,
        Family::SystemPlatform,
        Family::Journal,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_ids_are_distinct_and_all_registered() {
        let r = conformance_registry(vec![], vec![]);
        assert_eq!(r.len(), 6);
        assert_eq!(
            r.ids(),
            vec![REGISTERED, ENVELOPE, SCHEMA, CLASS, SECRETS, COVERAGE]
        );
    }

    #[test]
    fn a_forbidden_key_is_found_at_any_depth_by_path() {
        let v = serde_json::json!({"a": {"b": [{"password_hint": 1}]}});
        let mut out = Vec::new();
        forbidden_keys(&v, "", &mut out);
        assert_eq!(out, vec!["a.b[0].password_hint".to_owned()]);
    }
}
