//! WP-AI-11.T11: the AI timeline route over a running host and the test PostgreSQL (the tests
//! print a skip line and pass when `SPOT_TEST_PG_URL` is unset).
//!
//! The host is the sample site with the platform's roles: a Viewer reads Tasks but not the
//! confidential `details` group, the Administrator reads everything, and a stranger holds no role.
//! One AI action is chained on one Task through the hosted audit port, and the route
//! `GET /api/v1/resource/task/{id}/ai-timeline` is asked as each of them: the entry as recorded,
//! the entry with every value `***`, and `{"data":{"entries":[]}}` for a stranger, an unknown id,
//! a malformed id and an unknown DocType (the same bytes in every case).
//!
//! A store failure (the `503`) cannot be injected into a running host without breaking the
//! database under the harness; it is covered with a failing audit service in the adapter's unit
//! tests (`services::ai_audit::timeline_port`) and with a failing access in
//! `ai_audit_read_access.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use spot_app_api::prelude::RecordId;
use spot_cli::services::ai_audit::{ActionKind, AiAction, Outcome, hosted};

const VIEWER: &str = "viewer@example.com";
const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const STRANGER: &str = "stranger@example.com";
const STRANGER_PASSWORD: &str = "amber-thicket-vessel-6-Quill";
const PLANTED: &str = "PLANTED-CONFIDENTIAL-9917";
const AT_MICROS: i64 = 1_700_000_000_000_000;

struct World {
    env: Env,
    started: Started,
    admin: String,
    admin_id: String,
    viewer: String,
    stranger: String,
    task: String,
}

fn empty() -> Value {
    json!({ "data": { "entries": [] } })
}

/// A host with the AI audit service on, a Viewer, a Tenant Administrator, a stranger with no role,
/// one Task whose protected `details` the Viewer may not read and one AI action chained on it.
async fn world() -> Option<World> {
    let Some(env) = Env::new("[services.ai_audit]\nenabled = true\n").await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return None;
    };
    let first = env.site();
    env.user(VIEWER, VIEWER_PASSWORD);
    env.user(STRANGER, STRANGER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let admin_id = me(addr, &host, &admin).await;
    let viewer_id = user_id(addr, &host, &admin, VIEWER).await;
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
            "code": "PRJ-AIT-1", "title": "AI timeline", "status": "Active",
            "owner": viewer_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project = project.body["data"]["id"].as_str().unwrap().to_owned();
    let made = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Timeline task", "project": project, "details": PLANTED})),
    )
    .await;
    expect(&made, 201);
    let task = made.body["data"]["id"].as_str().unwrap().to_owned();

    let viewer = token_of(addr, &host, VIEWER, VIEWER_PASSWORD).await;
    let stranger = token_of(addr, &host, STRANGER, STRANGER_PASSWORD).await;

    let mut port = None;
    for _ in 0..100 {
        port = hosted(&host);
        if port.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let port = port.expect("the AI audit service is hosted");
    port.append(AiAction {
        kind: ActionKind::ToolCall,
        event_id: "ait-event-1".to_owned(),
        agent: "bot1".to_owned(),
        acting_user: "someone".to_owned(),
        tool: "update_task".to_owned(),
        arguments: vec![
            ("title".to_owned(), "plan".to_owned()),
            ("api_token".to_owned(), "tok-ait-secret".to_owned()),
        ],
        outcome: Outcome::Ok,
        subject: Some(("task".to_owned(), task.clone())),
        refers_to: None,
        at_micros: AT_MICROS,
    })
    .await
    .unwrap();
    Some(World {
        env,
        started,
        admin,
        admin_id,
        viewer,
        stranger,
        task,
    })
}

impl World {
    async fn timeline(&self, token: Option<&str>, doctype: &str, id: &str, query: &str) -> Reply {
        let path = if query.is_empty() {
            format!("/api/v1/resource/{doctype}/{id}/ai-timeline")
        } else {
            format!("/api/v1/resource/{doctype}/{id}/ai-timeline?{query}")
        };
        http(
            self.started.addr,
            &self.env.site,
            token,
            "GET",
            &path,
            &[],
            None,
        )
        .await
    }
}

fn arguments(entry: &Value) -> Vec<(String, String)> {
    entry["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap().to_owned(),
                a["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_of_everything_sees_the_entry_as_recorded() {
    let Some(w) = world().await else { return };
    let reply = w.timeline(Some(&w.admin), "task", &w.task, "").await;
    expect(&reply, 200);
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    let entries = reply.body["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{}", reply.body);
    let entry = &entries[0];
    assert_eq!(entry["kind"], "tool_call");
    assert_eq!(entry["agent"], "bot1");
    assert_eq!(entry["acting_user"], "someone");
    assert_eq!(entry["tool"], "update_task");
    assert_eq!(entry["outcome"], "ok");
    assert_eq!(entry["at"], "2023-11-14T22:13:20Z");
    assert!(entry["seq"].is_u64());
    let args = arguments(entry);
    assert!(
        args.contains(&("title".to_owned(), "plan".to_owned())),
        "{args:?}"
    );
    // A value the write-time mask hid stays hidden for everyone.
    assert!(
        args.contains(&("api_token".to_owned(), "***".to_owned())),
        "{args:?}"
    );
    let text = reply.body.to_string();
    assert!(
        !text.contains("tok-ait-secret") && !text.contains(PLANTED),
        "{text}"
    );
    // The answer is the entries and nothing else.
    assert_eq!(reply.body.as_object().unwrap().len(), 1);
    assert_eq!(reply.body["data"].as_object().unwrap().len(), 1);
    // The Administrator's own id is not in the answer either.
    assert!(!text.contains(&w.admin_id), "{text}");

    // `limit` bounds the answer.
    let one = w.timeline(Some(&w.admin), "task", &w.task, "limit=1").await;
    expect(&one, 200);
    assert_eq!(one.body["data"]["entries"].as_array().unwrap().len(), 1);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_of_the_document_but_not_every_field_sees_every_value_masked() {
    let Some(w) = world().await else { return };
    let reply = w.timeline(Some(&w.viewer), "task", &w.task, "").await;
    expect(&reply, 200);
    let entries = reply.body["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{}", reply.body);
    // The names, the kind, the times, the agent, the user and the outcome still show.
    assert_eq!(entries[0]["tool"], "update_task");
    assert_eq!(entries[0]["agent"], "bot1");
    assert_eq!(entries[0]["outcome"], "ok");
    let args = arguments(&entries[0]);
    assert_eq!(args.len(), 2);
    assert!(args.iter().all(|(_, value)| value == "***"), "{args:?}");
    let text = reply.body.to_string();
    assert!(
        !text.contains("plan") && !text.contains("tok-ait-secret") && !text.contains(PLANTED),
        "{text}"
    );
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_right_a_missing_document_and_a_malformed_id_are_one_and_the_same_empty_answer() {
    let Some(w) = world().await else { return };
    let missing = RecordId::new_v7().to_string();
    let cases = [
        // A viewer with no role, on a document that exists.
        (Some(w.stranger.as_str()), "task", w.task.as_str()),
        // A reader of everything on an id that is not in this site.
        (Some(w.admin.as_str()), "task", missing.as_str()),
        // A malformed id and an unknown DocType.
        (Some(w.admin.as_str()), "task", "not-a-record-id"),
        (Some(w.admin.as_str()), "task", "L3-1"),
        (Some(w.admin.as_str()), "no_such_doctype", w.task.as_str()),
    ];
    let mut bodies = Vec::new();
    for (token, doctype, id) in cases {
        let reply = w.timeline(token, doctype, id, "").await;
        expect(&reply, 200);
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
        assert_eq!(reply.body, empty(), "{doctype} {id}");
        bodies.push(reply.bytes);
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "an unreadable document answers the same bytes as a missing one"
    );
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_viewer_is_the_caller_and_never_a_query_value_or_a_body_field() {
    let Some(w) = world().await else { return };
    // A query value naming the Administrator is not an accepted parameter.
    let refused = w
        .timeline(
            Some(&w.viewer),
            "task",
            &w.task,
            &format!("viewer={}", w.admin_id),
        )
        .await;
    expect(&refused, 400);
    assert_eq!(refused.body["error"]["code"], "api.invalid_request");
    // A body naming the Administrator is ignored: the Viewer still sees the masked entry.
    let reply = http(
        w.started.addr,
        &w.env.site,
        Some(&w.viewer),
        "GET",
        &format!("/api/v1/resource/task/{}/ai-timeline", w.task),
        &[],
        Some(json!({ "viewer": w.admin_id, "user": w.admin_id })),
    )
    .await;
    expect(&reply, 200);
    let entries = reply.body["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{}", reply.body);
    assert!(
        arguments(&entries[0])
            .iter()
            .all(|(_, value)| value == "***"),
        "{}",
        reply.body
    );
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_limit_and_a_missing_bearer_are_refused_before_any_entry_is_read() {
    let Some(w) = world().await else { return };
    for query in ["limit=0", "limit=101", "limit=abc", "limit=1&limit=2"] {
        let reply = w.timeline(Some(&w.admin), "task", &w.task, query).await;
        expect(&reply, 400);
        assert_eq!(
            reply.body["error"]["code"], "api.invalid_request",
            "{query}"
        );
        assert!(reply.body.get("data").is_none(), "{query}");
    }
    let anonymous = w.timeline(None, "task", &w.task, "").await;
    expect(&anonymous, 401);
    assert!(anonymous.body.get("data").is_none());
    w.started.finish().await;
}
