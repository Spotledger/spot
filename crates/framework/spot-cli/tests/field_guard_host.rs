//! The field guard on a running `spot serve` (WP-SEC-04.T14), over real HTTP, real WebSocket and
//! the test PostgreSQL (skipped, with no output, when `SPOT_TEST_PG_URL` is unset).
//!
//! WP-SEC-04.T5, T6 and T8 built the guards; this suite proves the host installs them. A Viewer
//! may read tasks but not the `confidential` group (`task.details`); the Administrator may read
//! everything, which is the positive control: the planted value is on the wire for an entitled
//! caller, so a test that finds it missing for the Viewer is not looking at an empty table.
//!
//! There is no unguarded form of the host to run the same requests against (the list path, the
//! lookup and the pump are built guarded or not at all, by design), so the negative control of
//! the brief is the Administrator's replies plus the crate-level leakcheck suites that run the
//! same channels without the guard.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value as Json, json};
use spot_cli::live::{LiveProtection, SiteLive};
use spot_field_guard::ProtectionSource as _;

const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const PLANTED: &str = "PLANTED-CONFIDENTIAL-7731";

struct World {
    env: Env,
    started: Started,
    admin: String,
    viewer: String,
    project: String,
}

/// A host whose Task title is the protected `details` field, a Viewer who may not read it, and an
/// Administrator who may.
async fn world() -> Option<World> {
    let env = Env::new("").await?;
    // The title of a link to a Task is its `details`: a lookup typed against the title is a
    // lookup typed against a protected value.
    let task = env.root.0.join("sample/doctypes/task.toml");
    let text = std::fs::read_to_string(&task).unwrap();
    assert!(text.contains("title_field = \"subject\""), "no title field");
    std::fs::write(
        &task,
        text.replacen("title_field = \"subject\"", "title_field = \"details\"", 1),
    )
    .unwrap();
    let first = env.site();
    env.user("viewer@example.com", VIEWER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-FG-1", "title": "Field guard", "status": "Active",
            "owner": viewer_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project = project.body["data"]["id"].as_str().unwrap().to_owned();
    let viewer = token_of(addr, &host, "viewer@example.com", VIEWER_PASSWORD).await;
    Some(World {
        env,
        started,
        admin,
        viewer,
        project,
    })
}

impl World {
    async fn call(&self, token: &str, method: &str, path: &str, body: Option<Json>) -> Reply {
        http(
            self.started.addr,
            &self.env.site,
            Some(token),
            method,
            path,
            &[],
            body,
        )
        .await
    }

    async fn plant(&self, subject: &str) -> String {
        let made = self
            .call(
                &self.admin,
                "POST",
                "/api/v1/resource/task",
                Some(json!({"subject": subject, "project": self.project, "details": PLANTED})),
            )
            .await;
        expect(&made, 201);
        made.body["data"]["id"].as_str().unwrap().to_owned()
    }
}

fn wire(reply: &Reply) -> String {
    String::from_utf8_lossy(&reply.bytes).into_owned()
}

fn code(reply: &Reply) -> String {
    reply.body["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_list_never_puts_a_protected_column_on_the_wire_for_a_caller_who_may_not_read_it() {
    let Some(w) = world().await else { return };
    w.plant("Guarded task").await;

    // The entitled caller sees the value: the table is not empty and the column is real.
    let admin = w
        .call(
            &w.admin,
            "POST",
            "/api/v1/resource/task/query",
            Some(json!({})),
        )
        .await;
    expect(&admin, 200);
    assert!(wire(&admin).contains(PLANTED), "{}", wire(&admin));

    for (method, path, body) in [
        ("POST", "/api/v1/resource/task/query", Some(json!({}))),
        ("GET", "/api/v1/resource/task", None),
    ] {
        let viewer = w.call(&w.viewer, method, path, body).await;
        expect(&viewer, 200);
        let text = wire(&viewer);
        assert!(text.contains("Guarded task"), "the row is listed: {text}");
        assert!(!text.contains(PLANTED), "a protected value leaked: {text}");
    }

    // Projecting the protected column by name is answered as a name the DocType does not have
    // (WP-SEC-04.T5): the refusal does not tell a hidden field from an absent one.
    let hidden = w
        .call(
            &w.viewer,
            "POST",
            "/api/v1/resource/task/query",
            Some(json!({"fields": ["subject", "details"]})),
        )
        .await;
    let absent = w
        .call(
            &w.viewer,
            "POST",
            "/api/v1/resource/task/query",
            Some(json!({"fields": ["subject", "no_such_field_zz"]})),
        )
        .await;
    expect(&hidden, 422);
    assert_eq!(code(&hidden), "document.unknown_field", "{}", wire(&hidden));
    assert_eq!(hidden.status, absent.status);
    assert_eq!(code(&hidden), code(&absent));
    assert!(!wire(&hidden).contains(PLANTED), "{}", wire(&hidden));
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filter_or_sort_on_a_protected_field_answers_as_an_unknown_field() {
    let Some(w) = world().await else { return };
    w.plant("Guarded task").await;
    let query = |field: &str| {
        json!({
            "filter": {"field": field, "op": "eq", "value": PLANTED},
            "order": [{"field": field}],
        })
    };
    let unknown = w
        .call(
            &w.viewer,
            "POST",
            "/api/v1/resource/task/query",
            Some(query("no_such_field")),
        )
        .await;
    let protected = w
        .call(
            &w.viewer,
            "POST",
            "/api/v1/resource/task/query",
            Some(query("details")),
        )
        .await;
    assert_eq!(
        code(&unknown),
        "document.unknown_field",
        "{}",
        wire(&unknown)
    );
    assert_eq!(protected.status, unknown.status, "{}", wire(&protected));
    assert_eq!(code(&protected), code(&unknown), "{}", wire(&protected));
    assert!(!wire(&protected).contains(PLANTED));
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_lookup_typed_against_a_protected_title_answers_like_a_plain_miss() {
    let Some(w) = world().await else { return };
    let task = w.plant("Guarded task").await;
    let lookup = |token: String, text: &'static str| {
        let w = &w;
        async move {
            w.call(
                &token,
                "POST",
                "/api/v1/lookup/time_entry/task",
                Some(json!({"text": text})),
            )
            .await
        }
    };

    // The entitled caller finds the task by its title: the lookup works.
    let admin = lookup(w.admin.clone(), "PLANTED").await;
    expect(&admin, 200);
    assert!(wire(&admin).contains(PLANTED), "{}", wire(&admin));

    // The Viewer typing the same text gets exactly what typing text that matches nothing gets.
    let hit = lookup(w.viewer.clone(), "PLANTED").await;
    let miss = lookup(w.viewer.clone(), "zzz-matches-nothing").await;
    expect(&hit, 200);
    expect(&miss, 200);
    assert_eq!(
        hit.body["data"]["rows"],
        miss.body["data"]["rows"],
        "{}",
        wire(&hit)
    );
    assert!(!wire(&hit).contains(PLANTED), "{}", wire(&hit));

    // Browsing without typing: the row may show, but only by its id, never by the title.
    let browse = lookup(w.viewer.clone(), "").await;
    expect(&browse, 200);
    assert!(!wire(&browse).contains(PLANTED), "{}", wire(&browse));
    let _ = task;
    w.started.finish().await;
}

async fn events(ws: &mut Ws, wait: Duration, room: &str) -> Vec<Json> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return seen;
        }
        match ws.next(left).await {
            Frame::Text(m) if m["type"] == "event" && m["room"] == room => seen.push(m),
            Frame::Text(_) => {}
            Frame::Quiet => return seen,
            other => panic!("the connection was closed ({other:?})"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_room_payload_never_carries_a_protected_field_name_or_value() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let mut viewer = Ws::connect(addr, &host).await;
    viewer.sign_in(&w.viewer).await;
    assert_eq!(viewer.join("doctype:task").await["type"], "joined");

    let task = w.plant("Guarded task").await;
    let changed = w
        .call(
            &w.admin,
            "PATCH",
            &format!("/api/v1/resource/task/{task}"),
            Some(json!({"details": "second planted value", "subject": "Renamed"})),
        )
        .await;
    // A patch needs the version: re-read it and retry once when the first answer says so.
    if changed.status != 200 {
        let one = w
            .call(
                &w.admin,
                "GET",
                &format!("/api/v1/resource/task/{task}"),
                None,
            )
            .await;
        expect(&one, 200);
        let patched = http(
            addr,
            &host,
            Some(&w.admin),
            "PATCH",
            &format!("/api/v1/resource/task/{task}"),
            &[("If-Match", &one.etag)],
            Some(json!({"details": "second planted value", "subject": "Renamed"})),
        )
        .await;
        expect(&patched, 200);
    }

    let seen = events(&mut viewer, Duration::from_secs(4), "doctype:task").await;
    assert!(
        !seen.is_empty(),
        "the room heard nothing: the test proves nothing"
    );
    for event in &seen {
        let text = event.to_string();
        assert!(!text.contains("details"), "a protected field name: {text}");
        assert!(!text.contains(PLANTED), "a protected value: {text}");
        assert!(
            !text.contains("second planted"),
            "a protected value: {text}"
        );
    }
    w.started.finish().await;
}

#[test]
fn a_site_with_nothing_served_protects_nothing_it_knows_which_the_guard_reads_as_release_nothing() {
    // The realtime guard reads the served metadata per event. With nothing served the source
    // knows no DocType, and the guard releases nothing of an unknown DocType (fail closed).
    let source = LiveProtection::new(std::sync::Arc::new(SiteLive::new()));
    assert!(source.protection("task").is_none());
}
