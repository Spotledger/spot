//! The tower layer.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::convert::Infallible;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use common::{Buf, Line};
use http::{Request, Response, StatusCode};
use spot_core::SiteId;
use spot_telemetry::{
    RouteTemplate, SiteField, TelemetryLayer, cost, init_for_tests, render_prometheus,
};
use tower::{Layer, Service, ServiceExt, service_fn};

fn request(route: Option<&'static str>, site: Option<&str>) -> Request<()> {
    let mut req = Request::builder()
        .method("GET")
        .uri("/raw/123/path")
        .body(())
        .unwrap();
    if let Some(route) = route {
        req.extensions_mut().insert(RouteTemplate(route));
    }
    if let Some(site) = site {
        req.extensions_mut()
            .insert(SiteField(SiteId::new(site).unwrap()));
    }
    req
}

fn access_lines(buf: &Buf) -> Vec<Line> {
    buf.lines()
        .into_iter()
        .filter(|l| l["target"] == "spot_telemetry::access")
        .collect()
}

async fn handler(_req: Request<()>) -> Result<Response<()>, Infallible> {
    tokio::task::yield_now().await;
    cost::record_db(Duration::from_millis(1), 4, 2);
    cost::record_cache(true);
    cost::record_external_call();
    tracing::info!("inside handler");
    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(())
        .unwrap())
}

#[tokio::test]
async fn one_access_log_event_per_request_with_cost_fields() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let mut svc = TelemetryLayer::new().layer(service_fn(handler));
    let resp = svc
        .ready()
        .await
        .unwrap()
        .call(request(Some("/t/access"), Some("acme.example")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let access = access_lines(&buf);
    assert_eq!(access.len(), 1);
    let line = &access[0];
    assert_eq!(line["status"], "201");
    assert_eq!(line["method"], "GET");
    assert_eq!(line["route"], "/t/access");
    assert_eq!(line["site"], "acme.example");
    assert_eq!(line["db_statements"], "1");
    assert_eq!(line["db_time_us"], "1000");
    assert_eq!(line["rows_read"], "4");
    assert_eq!(line["rows_written"], "2");
    assert_eq!(line["cache_hits"], "1");
    assert_eq!(line["cache_misses"], "0");
    assert_eq!(line["outbox_events"], "0");
    assert_eq!(line["external_calls"], "1");
    assert!(line.contains_key("duration_us"));
    assert!(!line["request_id"].is_empty());

    // The handler's own log line is inside the request span.
    let inner = buf
        .lines()
        .into_iter()
        .find(|l| l["message"] == "inside handler")
        .unwrap();
    assert_eq!(inner["request_id"], line["request_id"]);
    assert_eq!(inner["site"], "acme.example");
    // The raw path never appears.
    assert!(!buf.text().contains("/raw/123"));
}

#[tokio::test]
async fn request_id_and_trace_id_come_from_headers() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let mut svc = TelemetryLayer::new().layer(service_fn(handler));
    let mut req = request(Some("/t/ids"), None);
    req.headers_mut()
        .insert("x-request-id", "req-42".parse().unwrap());
    req.headers_mut().insert(
        "traceparent",
        "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01"
            .parse()
            .unwrap(),
    );
    svc.ready().await.unwrap().call(req).await.unwrap();
    let line = &access_lines(&buf)[0];
    assert_eq!(line["request_id"], "req-42");
    assert_eq!(line["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert!(!line.contains_key("site"));
}

#[tokio::test]
async fn metrics_have_route_template_labels_and_no_site_label() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let mut svc = TelemetryLayer::new().layer(service_fn(handler));
    for _ in 0..2 {
        svc.ready()
            .await
            .unwrap()
            .call(request(Some("/t/metrics"), Some("tenant-a.example")))
            .await
            .unwrap();
    }
    svc.ready()
        .await
        .unwrap()
        .call(request(None, Some("tenant-b.example")))
        .await
        .unwrap();

    let out = render_prometheus();
    let counter = out
        .lines()
        .find(|l| l.starts_with("spot_http_requests_total{") && l.contains("route=\"/t/metrics\""))
        .unwrap_or_else(|| panic!("no counter in:\n{out}"));
    assert!(
        counter.contains("method=\"GET\"") && counter.contains("status=\"201\""),
        "{counter}"
    );
    assert!(counter.ends_with(" 2"), "{counter}");
    assert!(out.contains("route=\"unmatched\""));
    assert!(out.contains("spot_http_request_duration_seconds_bucket{"));
    assert!(out.contains("spot_db_statements_total"));
    assert!(out.contains("spot_db_time_seconds_bucket"));
    for forbidden in ["site=", "tenant-a", "tenant-b", "/raw/123"] {
        assert!(!out.contains(forbidden), "{forbidden} in metrics:\n{out}");
    }
}

#[tokio::test]
async fn panicking_service_still_logs_a_500_then_the_panic_continues() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let mut svc = TelemetryLayer::new().layer(service_fn(|_req: Request<()>| async {
        if true {
            panic!("boom");
        }
        Ok::<Response<()>, Infallible>(Response::new(()))
    }));
    let fut = svc
        .ready()
        .await
        .unwrap()
        .call(request(Some("/t/panic"), None));
    let caught = futures_catch(fut).await;
    assert!(caught.is_err(), "the panic must propagate");
    let access = access_lines(&buf);
    assert_eq!(access.len(), 1);
    assert_eq!(access[0]["status"], "500");
}

/// Polls a future under `catch_unwind` without a `futures` dependency.
async fn futures_catch<F: std::future::Future>(fut: F) -> Result<F::Output, ()> {
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    })
    .await
}

#[tokio::test]
async fn dropped_request_logs_once_as_cancelled() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let mut svc = TelemetryLayer::new().layer(service_fn(|_req: Request<()>| async {
        std::future::pending::<()>().await;
        Ok::<Response<()>, Infallible>(Response::new(()))
    }));
    let fut = svc
        .ready()
        .await
        .unwrap()
        .call(request(Some("/t/cancel"), None));
    let _ = tokio::time::timeout(Duration::from_millis(10), fut).await;
    let access = access_lines(&buf);
    assert_eq!(access.len(), 1);
    assert_eq!(access[0]["status"], "499");
}

#[tokio::test]
async fn service_error_counts_as_500() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let mut svc = TelemetryLayer::new().layer(service_fn(|_req: Request<()>| async {
        Err::<Response<()>, &'static str>("down")
    }));
    let result = svc
        .ready()
        .await
        .unwrap()
        .call(request(Some("/t/err"), None))
        .await;
    assert_eq!(result.unwrap_err(), "down");
    assert_eq!(access_lines(&buf)[0]["status"], "500");
}

#[tokio::test]
async fn concurrent_requests_do_not_mix_costs() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let layer = TelemetryLayer::new();
    let mut a = layer.layer(service_fn(|_r: Request<()>| async {
        for _ in 0..3 {
            tokio::task::yield_now().await;
            cost::record_db(Duration::ZERO, 1, 0);
        }
        Ok::<_, Infallible>(Response::new(()))
    }));
    let mut b = layer.layer(service_fn(|_r: Request<()>| async {
        tokio::task::yield_now().await;
        cost::record_db(Duration::ZERO, 1, 0);
        Ok::<_, Infallible>(Response::new(()))
    }));
    let fa = a
        .ready()
        .await
        .unwrap()
        .call(request(Some("/t/conc-a"), None));
    let fb = b
        .ready()
        .await
        .unwrap()
        .call(request(Some("/t/conc-b"), None));
    let (ra, rb) = tokio::join!(fa, fb);
    ra.unwrap();
    rb.unwrap();
    for line in access_lines(&buf) {
        match line["route"].as_str() {
            "/t/conc-a" => assert_eq!(line["db_statements"], "3"),
            "/t/conc-b" => assert_eq!(line["db_statements"], "1"),
            other => panic!("unexpected route {other}"),
        }
    }
}
