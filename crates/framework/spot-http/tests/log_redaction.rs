//! A secret in the path of an open route (a webhook token) never reaches a span, a record or an
//! event: the request span, the unknown-route fallback and the method-not-allowed fallback log
//! the route template, not the concrete path (WP-PRD-01.T54).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use spot_core::SiteId;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

const SECRET: &str = "tok-9f3a-distinctive-secret";
const TEMPLATE: &str = "/api/v1/hook/{token}";

/// Every captured field value, of spans, records and events, as `name=value` lines.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Lines<'a>(&'a mut Vec<String>);

impl Visit for Lines<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push(format!("{}={value}", field.name()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let mut lines = vec![format!("span {}", attrs.metadata().name())];
        attrs.record(&mut Lines(&mut lines));
        self.0.lock().unwrap().extend(lines);
    }

    fn on_record(&self, _id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut lines = Vec::new();
        values.record(&mut Lines(&mut lines));
        self.0.lock().unwrap().extend(lines);
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut lines = vec![format!("event {}", event.metadata().target())];
        event.record(&mut Lines(&mut lines));
        self.0.lock().unwrap().extend(lines);
    }
}

fn router() -> Router {
    let site = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add("acme.example", site.clone())
            .add_site_name(site),
    )
    .site_routes(Router::new().route(TEMPLATE, get(|| async { "ok" })))
    .build()
}

fn request(method: &str, path: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("host", "acme.example")
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn a_path_secret_never_reaches_a_span_record_or_event() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(
        Registry::default()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(capture.clone()),
    );
    let router = router();

    let ok = router
        .clone()
        .oneshot(request("GET", &format!("/api/v1/hook/{SECRET}?q={SECRET}")))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let matched_lines = capture.0.lock().unwrap().clone();
    assert!(
        matched_lines
            .iter()
            .any(|line| line == &format!("route={TEMPLATE}")),
        "the template is not logged: {matched_lines:?}"
    );

    let unknown = router
        .clone()
        .oneshot(request("GET", &format!("/nowhere/{SECRET}")))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let wrong_method = router
        .oneshot(request("POST", &format!("/api/v1/hook/{SECRET}")))
        .await
        .unwrap();
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);

    let lines = capture.0.lock().unwrap().clone();
    assert!(
        lines.iter().all(|line| !line.contains(SECRET)),
        "the secret reached the logs: {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.starts_with("path=")),
        "a path field is still logged: {lines:?}"
    );
}
