//! `app_list` (`GET /api/v1/apps`, WP-EXT-05.T3) over [`MemoryAppsPort`]: the administrator sees
//! the list, an ordinary user is 403, a user of another tenant sees only that tenant's apps,
//! requested versus granted capabilities and failures show as codes, paging and bad queries.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::apps::{AppState, AppsPort, MemoryAppsPort};
use spot_http::api::router::routes::{RouteState, apps};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const HOST_B: &str = "erp.beta.com";

fn router(port: Option<Arc<MemoryAppsPort>>) -> axum::Router {
    let acme = SiteId::new("acme").unwrap();
    let beta = SiteId::new("beta").unwrap();
    let sites = StaticSites::new()
        .add(HOST, acme.clone())
        .add(HOST_B, beta.clone())
        .add_site_name(acme)
        .add_site_name(beta);
    let mut ports = Ports::new();
    if let Some(port) = port {
        ports.provide::<dyn AppsPort>(port);
    }
    let state = RouteState::new(ports.subset(&[]).unwrap());
    let state = if ports.has(&apps::required_ports()[0]) {
        RouteState::new(ports.subset(apps::required_ports()).unwrap())
    } else {
        state
    };
    ServerBuilder::new(ServerConfig::default(), sites)
        .site_routes(apps::routes().with_state(state))
        .build()
}

fn fixture() -> Arc<MemoryAppsPort> {
    let port = Arc::new(MemoryAppsPort::new());
    let mut ok = MemoryAppsPort::simple("inventory");
    ok.capabilities_requested = vec!["network".to_owned(), "files".to_owned()];
    ok.capabilities_granted = vec!["files".to_owned()];
    port.add_app("acme", ok);
    let mut bad = MemoryAppsPort::simple("tampered");
    bad.state = AppState::Failed;
    bad.loaded = false;
    bad.last_error_code = Some("app.digest_mismatch".to_owned());
    port.add_app("acme", bad);
    port.add_app("acme", MemoryAppsPort::simple("zeta"));
    port.add_app("beta", MemoryAppsPort::simple("beta_only"));
    for site in ["acme", "beta"] {
        port.grant_admin(site, "tok-admin");
        port.grant_user(site, "tok-user");
    }
    port
}

async fn get(router: &axum::Router, host: &str, path: &str, token: Option<&str>) -> (u16, Json) {
    let mut builder = Request::builder().uri(path).header(header::HOST, host);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
}

#[tokio::test]
async fn the_administrator_sees_the_apps_with_capabilities_and_failure_codes() {
    let router = router(Some(fixture()));
    let (status, body) = get(&router, HOST, "/api/v1/apps", Some("tok-admin")).await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["inventory", "tampered", "zeta"]);
    assert_eq!(
        body["data"][0]["capabilities_requested"],
        serde_json::json!(["network", "files"])
    );
    assert_eq!(
        body["data"][0]["capabilities_granted"],
        serde_json::json!(["files"])
    );
    assert_eq!(body["data"][1]["state"], "failed");
    assert_eq!(body["data"][1]["loaded"], false);
    assert_eq!(body["data"][1]["last_error_code"], "app.digest_mismatch");
    assert!(body["next_cursor"].is_null());
}

#[tokio::test]
async fn a_normal_user_is_403_and_an_unknown_or_absent_credential_is_401() {
    let router = router(Some(fixture()));
    let (status, body) = get(&router, HOST, "/api/v1/apps", Some("tok-user")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (403, Some("permission.denied"))
    );
    assert_eq!(get(&router, HOST, "/api/v1/apps", None).await.0, 401);
    assert_eq!(
        get(&router, HOST, "/api/v1/apps", Some("nobody")).await.0,
        401
    );
}

#[tokio::test]
async fn a_tenant_sees_only_its_own_apps() {
    let router = router(Some(fixture()));
    let (status, body) = get(&router, HOST_B, "/api/v1/apps", Some("tok-admin")).await;
    assert_eq!(status, 200);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["name"], "beta_only");
}

#[tokio::test]
async fn paging_and_bad_queries() {
    let router = router(Some(fixture()));
    let (_, first) = get(&router, HOST, "/api/v1/apps?limit=2", Some("tok-admin")).await;
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, rest) = get(
        &router,
        HOST,
        &format!("/api/v1/apps?limit=2&cursor={cursor}"),
        Some("tok-admin"),
    )
    .await;
    assert_eq!(rest["data"][0]["name"], "zeta");
    for query in [
        "limit=0",
        "limit=101",
        "limit=1&limit=2",
        "x=1",
        "cursor=zz",
    ] {
        let (status, body) = get(
            &router,
            HOST,
            &format!("/api/v1/apps?{query}"),
            Some("tok-admin"),
        )
        .await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (400, Some("api.invalid_request")),
            "{query}"
        );
    }
}

#[tokio::test]
async fn with_no_port_the_answer_is_501_never_an_empty_list() {
    let (status, body) = get(&router(None), HOST, "/api/v1/apps", Some("tok-admin")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (501, Some("service.not_configured"))
    );
}
