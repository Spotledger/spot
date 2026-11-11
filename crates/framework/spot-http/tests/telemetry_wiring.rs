//! Telemetry wiring through the real server stack (WP-PLT-15.T2).
//!
//! One request drives a router built by [`ServerBuilder`]; its handler runs a PostgreSQL
//! transaction. A capture layer records every span and event, so the test can assert that one
//! trace has the HTTP request span as the ancestor of the `storage.tx` span, that the route is
//! the template (not the raw path), that the access event carries the per-request database cost,
//! and that the Prometheus metrics have the request and storage series.
//!
//! The database tests skip (printing the harness' own line) when no PostgreSQL server is
//! configured, exactly like the backend's own suites.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use spot_core::SiteId;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, Collation, Isolation, Name, StorageBackend, Transaction, TxOptions,
};
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// A secret-looking parameter: it must never reach the access log or a span.
const SECRET: &str = "super-secret-parameter-value";
/// The raw path carries an id; only the route template may be a label.
const RAW_PATH: &str = "/api/v1/things/42";
const TEMPLATE: &str = "/api/v1/things/{id}";

#[derive(Clone, Debug)]
struct SpanRecord {
    id: u64,
    parent: Option<u64>,
    name: &'static str,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct EventRecord {
    target: String,
    span: Option<u64>,
    fields: BTreeMap<String, String>,
}

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

    /// Walks the parent chain of `span`, from its direct parent outwards.
    fn ancestors(&self, span: &SpanRecord) -> Vec<SpanRecord> {
        let spans = self.spans();
        let mut out = Vec::new();
        let mut parent = span.parent;
        while let Some(id) = parent {
            let Some(found) = spans.iter().find(|candidate| candidate.id == id) else {
                break;
            };
            out.push(found.clone());
            parent = found.parent;
        }
        out
    }

    fn access_event(&self) -> Option<EventRecord> {
        self.events()
            .into_iter()
            .find(|event| event.target == "spot_telemetry::access")
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

fn tx_options() -> TxOptions {
    TxOptions::new(
        Name::new("telemetry_wiring").unwrap(),
        Isolation::ReadCommitted,
    )
}

/// The handler: one real storage transaction per request.
async fn handler(State(backend): State<Arc<PgBackend>>, Path(id): Path<String>) -> Response {
    let mut tx = backend.begin(catalog(), tx_options()).await.expect("begin");
    let rows = tx
        .raw_query("SELECT $1::text", &[&SECRET], None)
        .await
        .expect("select");
    assert_eq!(rows.len(), 1);
    tx.commit().await.expect("commit");
    (StatusCode::OK, format!("thing {id}")).into_response()
}

fn site_routes(backend: Arc<PgBackend>) -> Router {
    Router::new()
        .route(TEMPLATE, get(handler))
        .with_state(backend)
}

fn builder(backend: Arc<PgBackend>) -> ServerBuilder {
    let site = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add("acme.example", site.clone())
            .add_site_name(site),
    )
    .site_routes(site_routes(backend))
}

fn request(path: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "acme.example")
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn one_request_traces_http_and_storage_in_one_trace_with_its_cost() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = Arc::new(PgBackend::connect(db.config()).await.unwrap());

    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));

    let router = builder(Arc::clone(&backend)).build();
    let response = router.oneshot(request(RAW_PATH)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    // The request span carries the template, the site and the id the response reports.
    let http = capture.span("http_request").expect("the request span");
    assert_eq!(http.fields.get("route").map(String::as_str), Some(TEMPLATE));
    assert_eq!(http.fields.get("site").map(String::as_str), Some("acme"));
    assert_eq!(
        http.fields.get("request_id").map(String::as_str),
        Some(response_id.as_str())
    );

    // One trace: the transaction is a child of the request span, and its statements of it.
    let tx = capture.span("storage.tx").expect("the transaction span");
    assert_eq!(
        tx.fields.get("label").map(String::as_str),
        Some("telemetry_wiring")
    );
    let ancestors = capture.ancestors(&tx);
    assert!(
        ancestors.iter().any(|span| span.id == http.id),
        "the transaction is not in the request's trace: {ancestors:?}"
    );
    let query = capture
        .spans()
        .into_iter()
        .find(|span| {
            span.name == "storage.query"
                && span.fields.get("kind").map(String::as_str) == Some("raw_query")
        })
        .expect("the statement span");
    assert_eq!(query.parent, Some(tx.id));

    // The cost snapshot in the access log counts the database work of this request.
    let access = capture.access_event().expect("one access event");
    assert_eq!(access.span, Some(http.id));
    assert_eq!(access.fields.get("status").map(String::as_str), Some("200"));
    let statements: u64 = access.fields.get("db_statements").unwrap().parse().unwrap();
    assert!(statements >= 2, "access event: {access:?}");
    assert!(access.fields.contains_key("rows_read"));

    // The raw path with its id is not a label and does not reach the access log.
    assert!(
        !access.fields.values().any(|value| value.contains(RAW_PATH)),
        "the raw path reached the access event: {access:?}"
    );
    assert!(
        !access.fields.values().any(|value| value.contains(SECRET)),
        "a parameter reached the access event: {access:?}"
    );

    // Metrics: the request counter with the template, and the storage histogram.
    let metrics = spot_telemetry::render_prometheus();
    assert!(
        metrics.contains("spot_http_requests_total"),
        "no request counter: {metrics}"
    );
    assert!(
        metrics.contains("route=\"/api/v1/things/{id}\""),
        "the request counter has no template label: {metrics}"
    );
    assert!(
        metrics.contains("spot_db_time_seconds_bucket"),
        "no storage histogram: {metrics}"
    );
    assert!(
        !metrics.contains("acme.example"),
        "the site leaked into a metric label: {metrics}"
    );
}

#[tokio::test]
async fn an_unmatched_route_uses_the_unmatched_label_not_the_path() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = Arc::new(PgBackend::connect(db.config()).await.unwrap());

    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));

    let router = builder(backend).build();
    let response = router
        .oneshot(request("/api/v1/nothing/here/9876"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let http = capture.span("http_request").expect("the request span");
    assert_eq!(
        http.fields.get("route").map(String::as_str),
        Some(spot_telemetry::UNMATCHED_ROUTE)
    );
    let access = capture.access_event().expect("one access event");
    assert_eq!(access.fields.get("status").map(String::as_str), Some("404"));
    assert!(
        !access
            .fields
            .values()
            .any(|value| value.contains("/api/v1/nothing/here/9876")),
        "the raw path reached the access event: {access:?}"
    );
}
