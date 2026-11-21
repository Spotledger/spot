//! The `/api/v1/occupancy/*` routes (`occupancy_claim`, `occupancy_confirm`,
//! `occupancy_release`, `occupancy_availability`, WP-STO-14.T2) over the in-memory
//! [`InMemoryOccupancyPort`] with a manual clock: claims, conflicts, the half-open boundary, the
//! late-confirm rule, idempotent release, the holder rules, visibility of the holder in
//! availability, bounds and idempotent replay. The conformance walk proves the response shapes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::ports::occupancy::{InMemoryOccupancyPort, OccupancyLimits, OccupancyPort};
use spot_http::api::router::routes::{RouteState, occupancy};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use time::macros::datetime;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const SITE: &str = "acme";
const ALICE: &str = "0192f3c4-0000-7000-8000-00000000a001";
const BOB: &str = "0192f3c4-0000-7000-8000-00000000b001";
const STAFF: &str = "0192f3c4-0000-7000-8000-00000000c001";
const HOLDS: &str = "/api/v1/occupancy/room_hold/holds";

struct Fixture {
    router: axum::Router,
    clock: Arc<ManualClock>,
    port: Arc<InMemoryOccupancyPort>,
}

struct Reply {
    status: u16,
    bytes: Vec<u8>,
    cache: Option<String>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.bytes).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn fixture_with(limits: Option<OccupancyLimits>) -> Fixture {
    let clock = Arc::new(ManualClock::new(datetime!(2030-01-01 08:00:00 UTC)));
    let mut port = InMemoryOccupancyPort::new(clock.clone());
    if let Some(limits) = limits {
        port = port.with_limits(limits);
    }
    let port = Arc::new(port);
    for (token, user) in [("tok-alice", ALICE), ("tok-bob", BOB), ("tok-staff", STAFF)] {
        port.grant_token(SITE, token, user);
    }
    port.grant_staff(SITE, STAFF);
    port.add_entity(SITE, "room_hold");
    let mut ports = Ports::new();
    ports.provide::<dyn OccupancyPort>(port.clone());
    let state = RouteState::new(ports.subset(occupancy::required_ports()).unwrap());
    let acme = SiteId::new(SITE).unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(occupancy::routes().with_state(state))
    .build();
    Fixture {
        router,
        clock,
        port,
    }
}

fn fixture() -> Fixture {
    fixture_with(None)
}

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
    body: Option<Json>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json).unwrap())
        }
        None => Body::empty(),
    };
    let response = f
        .router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        bytes,
        cache,
    }
}

fn night(unit: &str, from: &str, to: &str) -> Json {
    json!({"unit": unit, "start": from, "end": to})
}

async fn claim(f: &Fixture, token: &str, key: &str, body: Json) -> Reply {
    send(f, "POST", HOLDS, Some(token), Some(key), Some(body)).await
}

#[tokio::test]
async fn a_claim_answers_201_with_the_callers_hold_and_never_names_another_holder() {
    let f = fixture();
    let r = claim(
        &f,
        "tok-alice",
        "k1",
        night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.cache.as_deref(), Some("private, no-store"));
    let hold = r.json()["data"].clone();
    assert_eq!(hold["state"], "tentative");
    assert_eq!(hold["holder"], format!("user:{ALICE}"));
    assert_eq!(hold["unit"], "u1");
    assert_eq!(
        hold["expires_at"], "2030-01-01T08:15:00Z",
        "the default ttl is 900 seconds"
    );
    // A body that names a holder is refused, not ignored.
    let mut with_holder = night("u2", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z");
    with_holder["holder"] = json!(format!("user:{BOB}"));
    let r = claim(&f, "tok-alice", "k2", with_holder).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.code(), "api.invalid_request");
    assert!(!r.text().contains(BOB));
}

#[tokio::test]
async fn an_overlapping_claim_is_409_without_leaking_the_other_hold_and_an_adjacent_one_succeeds() {
    let f = fixture();
    let first = claim(
        &f,
        "tok-alice",
        "a1",
        night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await;
    assert_eq!(first.status, 201);
    let clash = claim(
        &f,
        "tok-bob",
        "b1",
        night("u1", "2030-02-02T09:00:00Z", "2030-02-03T10:00:00Z"),
    )
    .await;
    assert_eq!(clash.status, 409, "{}", clash.text());
    assert_eq!(clash.code(), "occupancy.conflict");
    let text = clash.text();
    assert!(
        !text.contains(ALICE) && !text.contains("2030-02-01") && !text.contains("expires"),
        "{text}"
    );
    // end equals the next start: half-open, no overlap.
    let adjacent = claim(
        &f,
        "tok-bob",
        "b2",
        night("u1", "2030-02-02T10:00:00Z", "2030-02-03T10:00:00Z"),
    )
    .await;
    assert_eq!(adjacent.status, 201, "{}", adjacent.text());
    // Another unit is free.
    let other = claim(
        &f,
        "tok-bob",
        "b3",
        night("u2", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await;
    assert_eq!(other.status, 201);
}

#[tokio::test]
async fn a_claim_is_checked_before_the_port() {
    let f = fixture();
    let good = night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z");
    let no_key = send(
        &f,
        "POST",
        HOLDS,
        Some("tok-alice"),
        None,
        Some(good.clone()),
    )
    .await;
    assert_eq!(no_key.status, 400);
    assert_eq!(no_key.code(), "api.invalid_request");
    let bad_key = send(
        &f,
        "POST",
        HOLDS,
        Some("tok-alice"),
        Some("has space"),
        Some(good.clone()),
    )
    .await;
    assert_eq!(bad_key.status, 400);
    let cases: Vec<(&str, Json, u16, &str)> = vec![
        (
            "end not after start",
            night("u1", "2030-02-02T10:00:00Z", "2030-02-02T10:00:00Z"),
            422,
            "occupancy.invalid_range",
        ),
        (
            "span over the maximum",
            night("u1", "2030-02-01T00:00:00Z", "2030-04-01T00:00:00Z"),
            422,
            "occupancy.invalid_range",
        ),
        (
            "not an instant",
            night("u1", "tomorrow", "2030-02-02T10:00:00Z"),
            400,
            "api.invalid_request",
        ),
        (
            "empty unit",
            night("", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
            400,
            "api.invalid_request",
        ),
        (
            "unit with a control character",
            night("a\nb", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
            400,
            "api.invalid_request",
        ),
        (
            "missing end",
            json!({"unit": "u1", "start": "2030-02-01T14:00:00Z"}),
            400,
            "api.invalid_request",
        ),
        (
            "unknown member",
            json!({"unit": "u1", "start": "2030-02-01T14:00:00Z", "end": "2030-02-02T10:00:00Z", "x": 1}),
            400,
            "api.invalid_request",
        ),
        (
            "ttl zero",
            json!({"unit": "u1", "start": "2030-02-01T14:00:00Z", "end": "2030-02-02T10:00:00Z", "ttl_seconds": 0}),
            400,
            "api.invalid_request",
        ),
        (
            "ttl over the contract",
            json!({"unit": "u1", "start": "2030-02-01T14:00:00Z", "end": "2030-02-02T10:00:00Z", "ttl_seconds": 86401}),
            400,
            "api.invalid_request",
        ),
    ];
    for (what, body, status, code) in cases {
        let r = claim(&f, "tok-alice", "k-shape", body).await;
        assert_eq!(r.status, status, "{what}: {}", r.text());
        assert_eq!(r.code(), code, "{what}");
    }
    let tight = fixture_with(Some(OccupancyLimits {
        max_span: time::Duration::days(30),
        max_ttl_seconds: 3600,
    }));
    let long = json!({"unit": "u1", "start": "2030-02-01T14:00:00Z", "end": "2030-02-02T10:00:00Z", "ttl_seconds": 7200});
    let r = claim(&tight, "tok-alice", "k-ttl", long).await;
    assert_eq!(r.status, 422, "{}", r.text());
    assert_eq!(r.code(), "occupancy.ttl_too_long");
    let anonymous = send(&f, "POST", HOLDS, None, Some("k"), Some(good)).await;
    assert_eq!(anonymous.status, 401);
}

#[tokio::test]
async fn a_replay_with_the_same_key_returns_the_first_answer_and_a_different_body_conflicts() {
    let f = fixture();
    let body = night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z");
    let first = claim(&f, "tok-alice", "same", body.clone()).await;
    let second = claim(&f, "tok-alice", "same", body).await;
    assert_eq!(first.status, 201);
    assert_eq!(second.status, 201, "{}", second.text());
    assert_eq!(first.json(), second.json());
    let other = claim(
        &f,
        "tok-alice",
        "same",
        night("u1", "2030-03-01T14:00:00Z", "2030-03-02T10:00:00Z"),
    )
    .await;
    assert_eq!(other.status, 409);
    assert_eq!(other.code(), "api.idempotency_conflict");
}

#[tokio::test]
async fn confirm_follows_the_late_confirm_rule_and_is_the_holders_or_staffs() {
    let f = fixture();
    let hold = claim(
        &f,
        "tok-alice",
        "c1",
        night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await
    .json()["data"]
        .clone();
    let path = format!("{HOLDS}/{}/confirm", hold["id"].as_str().unwrap());
    let missing_key = send(&f, "POST", &path, Some("tok-alice"), None, None).await;
    assert_eq!(missing_key.status, 400);
    let foreign = send(&f, "POST", &path, Some("tok-bob"), Some("x1"), None).await;
    assert_eq!(foreign.status, 404, "{}", foreign.text());
    assert_eq!(foreign.code(), "document.not_found");
    // Expired and nobody took its place: it confirms.
    f.clock.advance(std::time::Duration::from_secs(1000));
    let ok = send(&f, "POST", &path, Some("tok-alice"), Some("x2"), None).await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(ok.json()["data"]["state"], "confirmed");
    assert_eq!(ok.json()["data"]["expires_at"], Json::Null);
    // Expired and replaced: 409 occupancy.hold_expired.
    let second = claim(
        &f,
        "tok-alice",
        "c2",
        night("u2", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await
    .json()["data"]
        .clone();
    f.clock.advance(std::time::Duration::from_secs(1000));
    let taken = claim(
        &f,
        "tok-bob",
        "c3",
        night("u2", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await;
    assert_eq!(
        taken.status,
        201,
        "the expired hold no longer blocks: {}",
        taken.text()
    );
    let late = send(
        &f,
        "POST",
        &format!("{HOLDS}/{}/confirm", second["id"].as_str().unwrap()),
        Some("tok-alice"),
        Some("x3"),
        None,
    )
    .await;
    assert_eq!(late.status, 409, "{}", late.text());
    assert_eq!(late.code(), "occupancy.hold_expired");
    // Staff with the confirm right may confirm another's hold.
    let staff = send(
        &f,
        "POST",
        &format!(
            "{HOLDS}/{}/confirm",
            taken.json()["data"]["id"].as_str().unwrap()
        ),
        Some("tok-staff"),
        Some("x4"),
        None,
    )
    .await;
    assert_eq!(staff.status, 200, "{}", staff.text());
    let bad_id = send(
        &f,
        "POST",
        &format!("{HOLDS}/not-an-id/confirm"),
        Some("tok-alice"),
        Some("x5"),
        None,
    )
    .await;
    assert_eq!(bad_id.status, 400);
}

#[tokio::test]
async fn release_is_idempotent_204_and_only_the_holder_or_staff_may_do_it() {
    let f = fixture();
    let hold = claim(
        &f,
        "tok-alice",
        "r1",
        night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await
    .json()["data"]
        .clone();
    let path = format!("{HOLDS}/{}", hold["id"].as_str().unwrap());
    let stranger = send(&f, "DELETE", &path, Some("tok-bob"), None, None).await;
    assert_eq!(stranger.status, 403, "{}", stranger.text());
    assert_eq!(stranger.code(), "occupancy.not_holder");
    for _ in 0..2 {
        let r = send(&f, "DELETE", &path, Some("tok-alice"), None, None).await;
        assert_eq!(r.status, 204, "{}", r.text());
        assert!(r.bytes.is_empty());
    }
    // The interval is free again.
    let again = claim(
        &f,
        "tok-bob",
        "r2",
        night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await;
    assert_eq!(again.status, 201);
    let unknown = send(
        &f,
        "DELETE",
        &format!("{HOLDS}/0192f3c4-0000-7000-8000-0000000fffff"),
        Some("tok-alice"),
        None,
        None,
    )
    .await;
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn availability_hides_the_holder_from_a_caller_who_cannot_read_the_rows() {
    let f = fixture();
    claim(
        &f,
        "tok-alice",
        "v1",
        night("u1", "2030-02-02T00:00:00Z", "2030-02-03T00:00:00Z"),
    )
    .await;
    let path = "/api/v1/occupancy/room_hold/availability?unit=u1&from=2030-02-01T00:00:00Z&to=2030-02-04T00:00:00Z";
    let plain = send(&f, "GET", path, Some("tok-bob"), None, None).await;
    assert_eq!(plain.status, 200, "{}", plain.text());
    let intervals = plain.json()["data"]["intervals"].clone();
    let statuses: Vec<&str> = intervals
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, vec!["free", "busy", "free"]);
    assert!(
        !plain.text().contains(ALICE) && !plain.text().contains("holder"),
        "{}",
        plain.text()
    );
    let staff = send(&f, "GET", path, Some("tok-staff"), None, None).await;
    let held = &staff.json()["data"]["intervals"][1];
    assert_eq!(held["status"], "held");
    assert_eq!(held["holder"], format!("user:{ALICE}"));
}

#[tokio::test]
async fn availability_bounds_the_window_and_the_parameters() {
    let f = fixture();
    let base = "/api/v1/occupancy/room_hold/availability";
    let over = send(
        &f,
        "GET",
        &format!("{base}?unit=u1&from=2030-01-01T00:00:00Z&to=2031-01-03T00:00:00Z"),
        Some("tok-bob"),
        None,
        None,
    )
    .await;
    assert_eq!(over.status, 422, "{}", over.text());
    assert_eq!(over.code(), "occupancy.invalid_range");
    let exactly = send(
        &f,
        "GET",
        &format!("{base}?unit=u1&from=2030-01-01T00:00:00Z&to=2031-01-02T00:00:00Z"),
        Some("tok-bob"),
        None,
        None,
    )
    .await;
    assert_eq!(
        exactly.status,
        200,
        "366 days is allowed: {}",
        exactly.text()
    );
    for bad in [
        "unit=u1&from=2030-01-02T00:00:00Z&to=2030-01-01T00:00:00Z",
        "from=2030-01-01T00:00:00Z&to=2030-01-02T00:00:00Z",
        "unit=u1&from=x&to=2030-01-02T00:00:00Z",
        "unit=u1&from=2030-01-01T00:00:00Z&to=2030-01-02T00:00:00Z&extra=1",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("{base}?{bad}"),
            Some("tok-bob"),
            None,
            None,
        )
        .await;
        assert!(
            r.status == 400 || r.status == 422,
            "{bad}: {} {}",
            r.status,
            r.text()
        );
    }
}

#[tokio::test]
async fn a_node_that_does_not_own_the_partition_answers_403_partition_not_owner() {
    let f = fixture();
    f.port.set_unowned(SITE, "room_hold");
    let r = claim(
        &f,
        "tok-alice",
        "p1",
        night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z"),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert_eq!(r.code(), "partition.not_owner");
}

#[tokio::test]
async fn an_unknown_entity_or_a_malformed_one_never_reaches_a_hold() {
    let f = fixture();
    let unknown = send(
        &f,
        "POST",
        "/api/v1/occupancy/nothing/holds",
        Some("tok-alice"),
        Some("k"),
        Some(night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z")),
    )
    .await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.code(), "document.not_found");
    let bad = send(
        &f,
        "POST",
        "/api/v1/occupancy/No%20Good/holds",
        Some("tok-alice"),
        Some("k"),
        Some(night("u1", "2030-02-01T14:00:00Z", "2030-02-02T10:00:00Z")),
    )
    .await;
    assert_eq!(bad.status, 400);
}
