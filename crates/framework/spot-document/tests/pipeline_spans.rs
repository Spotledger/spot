//! The document pipeline's spans (WP-PLT-15.T3).
//!
//! A capture layer records every span and event of an operation run by the engine, so the tests
//! can assert the tree (`doc.op` > `doc.phase` > `doc.step`, with a `storage.tx` below the
//! `doc.op`), the fields (names and counts only) and that no document value, secret or error
//! text ever appears in one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
mod flaky;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{meta, n, now};
use flaky::{Fault, FlakyBackend};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocumentPatch, ErrorClass, HookPoint, Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{Engine, RetryPolicy, V7Ids};
use spot_events_contract::lifecycle::Operation;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ErrorKind, StorageBackend};
use spot_storage_fake::FakeBackend;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// A secret-looking value: it must never reach a span field or an event.
const SECRET: &str = "sk_live_secret-7f3a";
/// Error text a step raises: it must never reach a span field either.
const ERROR_TEXT: &str = "the-error-text-must-stay-out-of-traces";

#[derive(Clone, Debug)]
struct SpanRecord {
    id: u64,
    parent: Option<u64>,
    name: &'static str,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct Capture {
    spans: Arc<Mutex<Vec<SpanRecord>>>,
    events: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

impl Capture {
    fn spans(&self) -> Vec<SpanRecord> {
        self.spans.lock().unwrap().clone()
    }

    fn named(&self, name: &str) -> Vec<SpanRecord> {
        self.spans()
            .into_iter()
            .filter(|span| span.name == name)
            .collect()
    }

    fn one(&self, name: &str) -> SpanRecord {
        let mut found = self.named(name);
        assert_eq!(found.len(), 1, "exactly one `{name}` span: {found:?}");
        found.remove(0)
    }

    fn parent_of(&self, span: &SpanRecord) -> Option<SpanRecord> {
        let parent = span.parent?;
        self.spans().into_iter().find(|s| s.id == parent)
    }

    fn is_descendant(&self, span: &SpanRecord, ancestor: &SpanRecord) -> bool {
        let mut current = self.parent_of(span);
        while let Some(parent) = current {
            if parent.id == ancestor.id {
                return true;
            }
            current = self.parent_of(&parent);
        }
        false
    }

    /// Every captured string: span names, field names and values, events.
    fn everything(&self) -> String {
        let mut out = String::new();
        for span in self.spans() {
            out.push_str(span.name);
            for (k, v) in span.fields {
                out.push_str(&format!(" {k}={v}"));
            }
            out.push('\n');
        }
        for event in self.events.lock().unwrap().iter() {
            out.push_str(&format!("{event:?}\n"));
        }
        out
    }

    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(Registry::default().with(self.clone()))
    }
}

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl<S> Layer<S> for Capture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = BTreeMap::new();
        attrs.record(&mut Fields(&mut fields));
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .map(|parent| parent.id().into_u64());
        self.spans.lock().unwrap().push(SpanRecord {
            id: id.into_u64(),
            parent,
            name: attrs.metadata().name(),
            fields,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut fields = BTreeMap::new();
        values.record(&mut Fields(&mut fields));
        if let Some(record) = self
            .spans
            .lock()
            .unwrap()
            .iter_mut()
            .find(|span| span.id == id.into_u64())
        {
            record.fields.extend(fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        self.events.lock().unwrap().push(fields);
    }
}

type StepFn =
    Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;

/// A controller of `customer` built from closures.
struct Probe {
    steps: Vec<(StepDecl, StepFn)>,
}

impl Probe {
    fn new() -> Self {
        Self { steps: Vec::new() }
    }

    fn step(mut self, name: &str, point: HookPoint, f: StepFn) -> Self {
        let decl = StepDecl {
            name: StepName::new(name).unwrap(),
            point,
            priority: 0,
            budget: Duration::from_secs(5),
        };
        self.steps.push((decl, f));
        self
    }

    fn ok(self, name: &str, phase: Phase, op: Operation) -> Self {
        let f: StepFn = Arc::new(|_ctx| Box::pin(async { Ok(()) }));
        self.step(name, at(phase, op), f)
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.iter().map(|(d, _)| d.clone()).collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match self.steps.iter().find(|(d, _)| &d.name == step) {
            Some((_, f)) => f(ctx),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

fn at(phase: Phase, op: Operation) -> HookPoint {
    HookPoint::of(phase, Target::Op(op))
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("spans.test").unwrap(),
        actor: Actor::System {
            component: n("span_tests"),
        },
        command_id: CommandId::new("cmd-9").ok(),
        language: None,
        now: now(),
    }
}

fn customer(name: &str) -> spot_app_api::Document {
    let mut doc = spot_app_api::Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

fn build<B: StorageBackend>(backend: B, probe: Probe) -> Engine<B> {
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", Arc::new(probe));
    Engine::new(
        backend,
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_retries: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    })
}

async fn fake(probe: Probe) -> Engine<FlakyBackend<FakeBackend>> {
    let engine = build(FlakyBackend::new(FakeBackend::new()), probe);
    engine.prepare().await.unwrap();
    engine
}

fn field<'a>(span: &'a SpanRecord, name: &str) -> &'a str {
    span.fields
        .get(name)
        .unwrap_or_else(|| panic!("`{}` has no field `{name}`: {span:?}", span.name))
}

fn all_steps() -> Probe {
    Probe::new()
        .ok("probe:derive", Phase::Before, Operation::Create)
        .ok("probe:check", Phase::Validate, Operation::Create)
        .ok("probe:react", Phase::After, Operation::Create)
        .ok("probe:notify", Phase::Committed, Operation::Create)
}

#[tokio::test]
async fn a_create_is_an_op_span_with_phase_and_step_spans_below_it() {
    let capture = Capture::default();
    let _guard = capture.install();
    let engine = fake(all_steps()).await;
    engine.create(&inv(), customer("Acme")).await.unwrap();

    let op = capture.one("doc.op");
    assert_eq!(field(&op, "doctype"), "customer");
    assert_eq!(field(&op, "operation"), "create");
    assert_eq!(field(&op, "attempt"), "1");
    assert_eq!(field(&op, "retries"), "0");
    assert_eq!(field(&op, "outcome"), "ok");
    assert_eq!(op.parent, None);

    let phases = capture.named("doc.phase");
    let names: Vec<&str> = phases.iter().map(|p| field(p, "phase")).collect();
    assert_eq!(
        names,
        ["before", "validate", "write", "after", "committed"],
        "one span per phase, in ADR-51 order"
    );
    for phase in &phases {
        assert_eq!(capture.parent_of(phase).unwrap().id, op.id);
    }

    let steps = capture.named("doc.step");
    let summary: Vec<(String, String, String)> = steps
        .iter()
        .map(|s| {
            (
                field(s, "point").to_owned(),
                field(s, "step").to_owned(),
                field(s, "app").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("before_create", "probe:derive", "probe"),
            ("validate_create", "probe:check", "probe"),
            ("after_create", "probe:react", "probe"),
            ("create_committed", "probe:notify", "probe"),
        ]
        .map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned()))
    );
    for (step, phase) in steps
        .iter()
        .zip(["before", "validate", "after", "committed"])
    {
        let parent = capture.parent_of(step).unwrap();
        assert_eq!(parent.name, "doc.phase");
        assert_eq!(field(&parent, "phase"), phase);
    }
}

#[tokio::test]
async fn a_retried_operation_is_one_op_span_with_the_attempt_and_the_retry_count() {
    let capture = Capture::default();
    let _guard = capture.install();
    let engine = fake(Probe::new()).await;
    for _ in 0..2 {
        engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::SerializationFailure));
    }
    engine.create(&inv(), customer("Acme")).await.unwrap();
    let op = capture.one("doc.op");
    assert_eq!(field(&op, "attempt"), "3");
    assert_eq!(field(&op, "retries"), "2");
    assert_eq!(field(&op, "outcome"), "ok");
}

#[tokio::test]
async fn the_outcome_tells_refused_stale_and_failed_apart() {
    let capture = Capture::default();
    let _guard = capture.install();
    let refuse: StepFn = Arc::new(|_ctx| {
        Box::pin(async { Err(AppError::new(ErrorClass::Validation, "x.no", ERROR_TEXT)) })
    });
    let engine = fake(Probe::new().step(
        "probe:refuse",
        at(Phase::Validate, Operation::Delete),
        refuse,
    ))
    .await;

    let created = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    let version = created.version().unwrap();
    let patch = || DocumentPatch {
        fields: [(n("name"), Value::Text("Two".into()))].into(),
        ..Default::default()
    };
    let updated = engine
        .update(&inv(), "customer", created.id(), patch(), version)
        .await
        .unwrap()
        .value;
    engine
        .update(&inv(), "customer", created.id(), patch(), version)
        .await
        .unwrap_err();
    engine
        .delete(&inv(), "customer", created.id(), updated.version().unwrap())
        .await
        .unwrap_err();
    for _ in 0..4 {
        engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Deadlock));
    }
    engine.create(&inv(), customer("Three")).await.unwrap_err();

    let outcomes: Vec<(String, String)> = capture
        .named("doc.op")
        .iter()
        .map(|s| {
            (
                field(s, "operation").to_owned(),
                field(s, "outcome").to_owned(),
            )
        })
        .collect();
    let expected = [
        ("create", "ok"),
        ("update", "ok"),
        ("update", "stale"),
        ("delete", "refused"),
        ("create", "failed"),
    ]
    .map(|(a, b)| (a.to_owned(), b.to_owned()));
    assert_eq!(outcomes, expected);
    let last = capture.named("doc.op").pop().unwrap();
    assert_eq!(field(&last, "retries"), "3");
    assert!(!capture.everything().contains(ERROR_TEXT));
}

#[tokio::test]
async fn no_value_secret_or_error_text_reaches_a_span_field_or_an_event() {
    let capture = Capture::default();
    let _guard = capture.install();
    let refuse: StepFn = Arc::new(|_ctx| {
        Box::pin(async { Err(AppError::new(ErrorClass::Validation, "x.no", ERROR_TEXT)) })
    });
    let engine = fake(all_steps().step(
        "probe:refuse",
        at(Phase::Validate, Operation::Update),
        refuse,
    ))
    .await;
    let created = engine.create(&inv(), customer(SECRET)).await.unwrap().value;
    let patch = DocumentPatch {
        fields: [(n("name"), Value::Text(SECRET.into()))].into(),
        ..Default::default()
    };
    let error = engine
        .update(
            &inv(),
            "customer",
            created.id(),
            patch,
            created.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "x.no", "the span did not change the error");
    assert_eq!(error.message, ERROR_TEXT);

    let seen = capture.everything();
    assert!(!capture.spans().is_empty());
    for forbidden in [SECRET, ERROR_TEXT, "Acme", &created.id().to_string()] {
        assert!(
            !seen.contains(forbidden),
            "`{forbidden}` leaked into:\n{seen}"
        );
    }
    // Only these field names ever appear: names and counts.
    let allowed = [
        "doctype",
        "operation",
        "attempt",
        "retries",
        "outcome",
        "phase",
        "point",
        "step",
        "app",
    ];
    for span in capture.spans() {
        for key in span.fields.keys() {
            assert!(
                allowed.contains(&key.as_str()),
                "unexpected field `{key}` on {}",
                span.name
            );
        }
    }
}

#[tokio::test]
async fn a_storage_transaction_span_is_below_the_op_span_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let engine = build(backend, all_steps());
    engine.prepare().await.unwrap();

    let capture = Capture::default();
    let _guard = capture.install();
    engine.create(&inv(), customer("Acme")).await.unwrap();

    let op = capture.one("doc.op");
    let tx = capture.named("storage.tx");
    assert!(!tx.is_empty(), "the backend opened a transaction");
    for span in &tx {
        assert!(
            capture.is_descendant(span, &op),
            "`storage.tx` is below `doc.op`"
        );
    }
    let queries = capture.named("storage.query");
    assert!(!queries.is_empty());
    assert!(queries.iter().all(|q| capture.is_descendant(q, &op)));
    assert!(!capture.everything().contains("Acme"));
}
