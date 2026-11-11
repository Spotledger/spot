//! Storage spans and per-request cost (WP-PLT-15.T2).
//!
//! The spans are captured with a small `tracing` layer: one record per span and per event. The
//! tests prove that a transaction and each statement are spanned, that the statement span is a
//! child of the transaction span, that the cost accumulator sees the statements and their rows,
//! and that no SQL text or parameter value reaches a span or a log event.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, Collation, ErrorKind, Isolation, Name, StorageBackend, StorageError, Transaction,
    TxOptions,
};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// A secret-looking parameter value: it must never appear in a span or an event.
const SECRET: &str = "super-secret-parameter-value";
/// SQL text that must never appear either.
const SQL: &str = "SELECT $1::text";

#[derive(Clone, Debug)]
struct SpanRecord {
    id: u64,
    parent: Option<u64>,
    name: &'static str,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct EventRecord {
    target: String,
    span: Option<u64>,
    fields: BTreeMap<String, String>,
}

/// The captured spans and events, shared with the installed layer.
#[derive(Clone, Default)]
struct Capture {
    spans: Arc<Mutex<Vec<SpanRecord>>>,
    events: Arc<Mutex<Vec<EventRecord>>>,
}

impl Capture {
    fn spans(&self) -> Vec<SpanRecord> {
        self.spans.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<EventRecord> {
        self.events.lock().unwrap().clone()
    }

    fn span(&self, name: &str) -> Option<SpanRecord> {
        self.spans().into_iter().find(|span| span.name == name)
    }

    fn parent_of(&self, span: &SpanRecord) -> Option<SpanRecord> {
        let spans = self.spans();
        spans
            .into_iter()
            .find(|candidate| Some(candidate.id) == span.parent)
    }

    /// Every captured field value and event message, for the "nothing leaked" assertion.
    fn text(&self) -> String {
        let mut out = String::new();
        for span in self.spans() {
            out.push_str(span.name);
            out.push('\n');
            for (key, value) in span.fields {
                out.push_str(&key);
                out.push('=');
                out.push_str(&value);
                out.push('\n');
            }
        }
        for event in self.events() {
            out.push_str(&event.target);
            out.push('\n');
            for (key, value) in event.fields {
                out.push_str(&key);
                out.push('=');
                out.push_str(&value);
                out.push('\n');
            }
        }
        out
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

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
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

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        let span = ctx.event_span(event).map(|span| span.id().into_u64());
        self.events.lock().unwrap().push(EventRecord {
            target: event.metadata().target().to_owned(),
            span,
            fields,
        });
    }
}

fn catalog() -> Arc<Catalog> {
    Arc::new(Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap())
}

fn options() -> TxOptions {
    TxOptions::new(
        Name::new("telemetry_test").unwrap(),
        Isolation::ReadCommitted,
    )
}

/// Opens a scratch database, or returns `None` (with the harness' own skip line) without one.
async fn database() -> Option<(TestDb, PgBackend)> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    Some((db, backend))
}

#[tokio::test]
async fn a_transaction_and_each_statement_are_spanned_with_their_cost() {
    let Some((_db, backend)) = database().await else {
        return;
    };
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));

    let (result, cost) = spot_telemetry::cost::scope(async {
        let mut tx = backend.begin(catalog(), options()).await.unwrap();
        let rows = tx.raw_query(SQL, &[&SECRET], None).await.unwrap();
        assert_eq!(rows.len(), 1);
        tx.commit().await.unwrap();
        Ok::<(), StorageError>(())
    })
    .await;
    result.unwrap();

    let tx_span = capture.span("storage.tx").unwrap();
    assert_eq!(
        tx_span.fields.get("label").map(String::as_str),
        Some("telemetry_test")
    );
    let query_span = capture.span("storage.query").unwrap();
    assert_eq!(
        query_span.fields.get("kind").map(String::as_str),
        Some("raw_query")
    );
    assert_eq!(
        query_span.fields.get("label").map(String::as_str),
        Some("telemetry_test")
    );
    // The statement span hangs off the transaction span, and the commit is a statement too.
    let parent = capture.parent_of(&query_span).unwrap();
    assert_eq!(parent.id, tx_span.id);
    assert_eq!(parent.name, "storage.tx");
    assert!(
        capture
            .spans()
            .iter()
            .any(|span| span.name == "storage.query"
                && span.fields.get("kind").map(String::as_str) == Some("commit")),
        "the commit is spanned as a statement"
    );

    // The cost accumulator saw the transaction and its statements, and the rows the query read.
    assert!(cost.db_statements >= 3, "cost: {cost:?}");
    assert!(cost.rows_read >= 1, "cost: {cost:?}");
    assert!(!cost.db_time.is_zero(), "cost: {cost:?}");

    // Nothing the database saw may reach a span or an event.
    let text = capture.text();
    assert!(
        !text.contains(SQL),
        "SQL text leaked into telemetry: {text}"
    );
    assert!(
        !text.contains(SECRET),
        "a parameter leaked into telemetry: {text}"
    );
    assert!(!text.to_lowercase().contains("insert into"));
}

#[tokio::test]
async fn a_failed_statement_is_still_spanned_and_recorded() {
    let Some((_db, backend)) = database().await else {
        return;
    };
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));

    let (result, cost) = spot_telemetry::cost::scope(async {
        let mut tx = backend.begin(catalog(), options()).await.unwrap();
        let failure = tx
            .raw_query("SELECT * FROM a_table_that_does_not_exist", &[], None)
            .await;
        let kind = failure.unwrap_err().kind;
        let _ = tx.rollback().await;
        kind
    })
    .await;

    assert_eq!(result, ErrorKind::Internal);
    assert!(cost.db_statements >= 2, "cost: {cost:?}");
    assert!(
        capture
            .spans()
            .iter()
            .any(|span| span.name == "storage.query"
                && span.fields.get("kind").map(String::as_str) == Some("raw_query")),
        "the failed statement is spanned"
    );
    assert!(
        capture
            .spans()
            .iter()
            .any(|span| span.name == "storage.query"
                && span.fields.get("kind").map(String::as_str) == Some("rollback")),
        "the rollback is spanned"
    );
    // The server's own error message may name the relation (the driver error is logged by the
    // crate's existing error path); the statement span must not carry the SQL or the value.
    let query = capture
        .spans()
        .into_iter()
        .filter(|span| span.name == "storage.query")
        .find(|span| span.fields.get("kind").map(String::as_str) == Some("raw_query"))
        .unwrap();
    assert!(!query.fields.values().any(|value| value.contains(SECRET)));
    assert!(!query.fields.values().any(|value| value.contains("SELECT")));
}
