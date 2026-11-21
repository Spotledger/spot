//! WP-SEC-10.T8: the host serves `field_reveal` (`POST /api/v1/resource/{doctype}/{id}/reveal`)
//! over the real engine, the real permission engine and the real sign-in service. Real PostgreSQL,
//! a real `spot serve`, real HTTP (skipped without `SPOT_TEST_PG_URL`).
//!
//! The sample app's `Task.details` is in the traced protection group `confidential` (the
//! fixture patch of `tests/common`). A Project Manager is given the field's `reveal` right here;
//! a Viewer holds nothing in the group. What is proved: the holder reads one field of one
//! document with a reason; the answer is `no-store` and carries the value only on `200`; every
//! refusal (no right, a plain field, an unknown field, an unknown document, an unknown DocType)
//! is the one answer; a missing or over-long reason is refused by the route; two calls with one
//! `Idempotency-Key` both read; every call leaves one audit event, written before the answer, and
//! no event or error carries the value.
//!
//! WP-SEC-10.T9: the caller's reason is part of that durable event, verbatim, for an allowed and
//! for a refused call; it validates against the registered schema of `identity_field_accessed`
//! (events contract 0.7.9); a reason of 500 characters is stored whole; and the log holds neither
//! the reason nor the value.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use common::*;
use serde_json::{Value as Json, json};
use spot_events_contract::identity;
use spot_storage_contract::{ChangeFeed, FeedPosition};

const PLANTED: &str = "PLANTED-REVEAL-VALUE-5519";
const PM_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const VIEWER_PASSWORD: &str = "tangerine-harbor-lamp-3-Slate";
const REASON: &str = "Customer asked about the note on ticket 4521.";
/// Distinct texts for the log capture, so no other test's call can match them.
const LOGGED_REASON_ALLOWED: &str = "Reason text for the allowed call, distinct marker 7731-a.";
const LOGGED_REASON_REFUSED: &str = "Reason text for the refused call, distinct marker 7731-b.";

// ---- a capturing tracing subscriber (no extra dependency) --------------------------------

static CAPTURED: OnceLock<Arc<Mutex<String>>> = OnceLock::new();

struct Capture {
    out: Arc<Mutex<String>>,
    next: AtomicU64,
}

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = format!("span {}", attributes.metadata().name());
        attributes.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
        tracing::span::Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::from("record");
        values.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("event {}", event.metadata().target());
        event.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Installs the capturing subscriber once for this test binary and returns its buffer.
fn capture() -> Arc<Mutex<String>> {
    CAPTURED
        .get_or_init(|| {
            let out = Arc::new(Mutex::new(String::new()));
            let _ = tracing::subscriber::set_global_default(Capture {
                out: out.clone(),
                next: AtomicU64::new(0),
            });
            out
        })
        .clone()
}

struct World {
    env: Env,
    started: Started,
    manager: String,
    viewer: String,
    task: String,
}

impl World {
    async fn call(&self, token: &str, method: &str, path: &str, body: Option<Json>) -> Reply {
        self.call_with(token, method, path, &[], body).await
    }

    async fn call_with(
        &self,
        token: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Json>,
    ) -> Reply {
        http(
            self.started.addr,
            &self.env.site,
            Some(token),
            method,
            path,
            headers,
            body,
        )
        .await
    }

    async fn reveal(&self, token: &str, doctype: &str, id: &str, body: Json) -> Reply {
        self.call(
            token,
            "POST",
            &format!("/api/v1/resource/{doctype}/{id}/reveal"),
            Some(body),
        )
        .await
    }

    /// Every outbox event, as (topic, payload).
    async fn events(&self) -> Vec<(String, Json)> {
        let loaded = spot_cli::config::load(&self.env.root.0, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
        backend
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|record| {
                (
                    record.event.topic.as_str().to_owned(),
                    serde_json::from_str(&record.event.payload).unwrap_or(Json::Null),
                )
            })
            .collect()
    }

    /// The `data` of every field access trace of `field`.
    async fn traces(&self, field: &str) -> Vec<Json> {
        self.events()
            .await
            .into_iter()
            .filter(|(topic, payload)| {
                topic == "identity_field_accessed" && payload["data"]["field"] == field
            })
            .map(|(_, payload)| payload["data"].clone())
            .collect()
    }
}

fn code(reply: &Reply) -> String {
    reply.body["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn wire(reply: &Reply) -> String {
    String::from_utf8_lossy(&reply.bytes).into_owned()
}

/// The error body without the per-request id, so two answers can be compared.
fn shape(reply: &Reply) -> (u16, Json) {
    let mut body = reply.body.clone();
    if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
        error.remove("request_id");
    }
    (reply.status, body)
}

async fn world() -> Option<World> {
    let env = Env::new("").await?;
    // The Project Manager holds `reveal` on the `confidential` group of Task; the Viewer nothing.
    let grants = env.root.0.join("sample/permissions/field_grants.toml");
    let text = std::fs::read_to_string(&grants).unwrap();
    let from = "access = [\"read\", \"update\", \"report\"]";
    assert!(text.contains(from), "the sample field grant moved");
    std::fs::write(
        &grants,
        text.replacen(
            from,
            "access = [\"read\", \"update\", \"report\", \"reveal\"]",
            1,
        ),
    )
    .unwrap();
    let first = env.site();
    env.user("manager@example.com", PM_PASSWORD);
    env.user("viewer@example.com", VIEWER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let manager_id = user_id(addr, &host, &admin, "manager@example.com").await;
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let pm_role = role(addr, &host, &admin, "Project Manager").await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &manager_id, &pm_role).await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-RV-1", "title": "Reveal", "status": "Active",
            "owner": manager_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project = project.body["data"]["id"].as_str().unwrap().to_owned();
    let task = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Reveal me", "project": project, "details": PLANTED})),
    )
    .await;
    expect(&task, 201);
    let task = task.body["data"]["id"].as_str().unwrap().to_owned();
    let manager = token_of(addr, &host, "manager@example.com", PM_PASSWORD).await;
    let viewer = token_of(addr, &host, "viewer@example.com", VIEWER_PASSWORD).await;
    Some(World {
        env,
        started,
        manager,
        viewer,
        task,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_of_the_reveal_right_reads_one_field_with_a_reason_and_it_is_audited() {
    let Some(w) = world().await else { return };
    // The events the setup wrote (the task was created with the value) are not the reveal's.
    let before = w.events().await.len();
    let reply = w
        .reveal(
            &w.manager,
            "task",
            &w.task,
            json!({ "field": "details", "reason": REASON }),
        )
        .await;
    expect(&reply, 200);
    assert_eq!(reply.body["data"]["value"], PLANTED);
    assert_eq!(reply.header("cache-control"), Some("no-store"));

    // One trace for the call: who, document, group, field, the right, allowed, the door; and
    // never the value.
    let traces = w.traces("details").await;
    assert_eq!(traces.len(), 1, "{traces:?}");
    let trace = &traces[0];
    assert_eq!(trace["doctype"], "task");
    assert_eq!(trace["document"], json!(w.task));
    assert_eq!(trace["group"], "confidential");
    assert_eq!(trace["right"], "reveal");
    assert_eq!(trace["authorised"], json!(true));
    assert_eq!(trace["channel"], "api");
    // The reason is in the durable event, verbatim, and the event is valid against the registry.
    assert_eq!(trace["reason"], REASON);
    identity::validate("identity_field_accessed", trace).unwrap();
    for (topic, payload) in w.events().await.into_iter().skip(before) {
        assert!(
            !payload.to_string().contains(PLANTED),
            "the value is in the `{topic}` event"
        );
    }
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_without_the_right_is_refused_and_the_refusal_is_audited_too() {
    let Some(w) = world().await else { return };
    let reply = w
        .reveal(
            &w.viewer,
            "task",
            &w.task,
            json!({ "field": "details", "reason": REASON }),
        )
        .await;
    assert_eq!(reply.status, 403, "{}", wire(&reply));
    assert_eq!(code(&reply), "permission.reveal_denied");
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert!(!wire(&reply).contains(PLANTED));

    let traces = w.traces("details").await;
    assert_eq!(traces.len(), 1, "{traces:?}");
    assert_eq!(traces[0]["authorised"], json!(false));
    assert_eq!(traces[0]["right"], "reveal");
    // A refused call carries the reason too: the caller sent it either way.
    assert_eq!(traces[0]["reason"], REASON);
    identity::validate("identity_field_accessed", &traces[0]).unwrap();
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_other_refusal_is_the_same_answer() {
    let Some(w) = world().await else { return };
    let denied = w
        .reveal(
            &w.viewer,
            "task",
            &w.task,
            json!({ "field": "details", "reason": REASON }),
        )
        .await;
    assert_eq!(denied.status, 403, "{}", wire(&denied));
    let nobody = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6b";
    for (doctype, id, field) in [
        // A plain field has nothing to reveal, even for the holder of the right on another.
        ("task", w.task.as_str(), "subject"),
        // An unknown field, a path into a child table, an unknown document, an unknown DocType.
        ("task", w.task.as_str(), "no_such_field"),
        ("task", w.task.as_str(), "checklist[1].item"),
        ("task", nobody, "details"),
        ("no_such_doctype", w.task.as_str(), "details"),
    ] {
        let reply = w
            .reveal(
                &w.manager,
                doctype,
                id,
                json!({ "field": field, "reason": REASON }),
            )
            .await;
        assert_eq!(
            shape(&reply),
            shape(&denied),
            "{doctype}/{id}/{field}: {}",
            wire(&reply)
        );
        assert!(!wire(&reply).contains(PLANTED));
    }
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_or_overlong_reason_is_refused_by_the_route() {
    let Some(w) = world().await else { return };
    for body in [
        json!({ "field": "details" }),
        json!({ "field": "details", "reason": "" }),
        json!({ "field": "details", "reason": "x".repeat(501) }),
    ] {
        let reply = w.reveal(&w.manager, "task", &w.task, body).await;
        assert_eq!(reply.status, 400, "{}", wire(&reply));
        assert_eq!(code(&reply), "api.invalid_request");
        assert!(!wire(&reply).contains(PLANTED));
    }
    // A reason at the limit is fine, and it is stored whole: 500 characters, and 500 multi-byte
    // characters (the bound counts characters, not bytes).
    let multibyte = "\u{00e9}".repeat(500);
    for reason in ["x".repeat(500), multibyte] {
        let reply = w
            .reveal(
                &w.manager,
                "task",
                &w.task,
                json!({ "field": "details", "reason": reason }),
            )
            .await;
        expect(&reply, 200);
    }
    let traces = w.traces("details").await;
    assert_eq!(
        traces.len(),
        2,
        "only the two accepted calls were audited: {traces:?}"
    );
    assert_eq!(traces[0]["reason"].as_str().unwrap(), "x".repeat(500));
    assert_eq!(traces[1]["reason"].as_str().unwrap().chars().count(), 500);
    assert_eq!(traces[1]["reason"], json!("\u{00e9}".repeat(500)));
    for trace in &traces {
        identity::validate("identity_field_accessed", trace).unwrap();
    }
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idempotency_key_is_ignored_and_both_calls_read() {
    let Some(w) = world().await else { return };
    for _ in 0..2 {
        let reply = w
            .call_with(
                &w.manager,
                "POST",
                &format!("/api/v1/resource/task/{}/reveal", w.task),
                &[(
                    "Idempotency-Key",
                    concat!("0190a1b2-c3d4-7e5f-8a6b-", "1c2d3e4f5a6c"),
                )],
                Some(json!({ "field": "details", "reason": REASON })),
            )
            .await;
        expect(&reply, 200);
        assert_eq!(reply.body["data"]["value"], PLANTED);
        assert!(reply.header("idempotency-replayed").is_none());
    }
    // Two reads, two audit events.
    assert_eq!(w.traces("details").await.len(), 2);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_anonymous_call_is_refused_before_anything_is_read() {
    let Some(w) = world().await else { return };
    let reply = http(
        w.started.addr,
        &w.env.site,
        None,
        "POST",
        &format!("/api/v1/resource/task/{}/reveal", w.task),
        &[],
        Some(json!({ "field": "details", "reason": REASON })),
    )
    .await;
    assert_eq!(reply.status, 401, "{}", wire(&reply));
    assert!(w.traces("details").await.is_empty(), "no read, no trace");
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reason_is_durable_and_neither_it_nor_the_value_is_in_the_log() {
    let log = capture();
    let Some(w) = world().await else { return };
    // The set-up wrote the planted value into the document, so its own `doc_state` events hold
    // it: only the events after this point can be asked to hold neither reason nor value.
    let before = w.events().await.len();
    let allowed = w
        .reveal(
            &w.manager,
            "task",
            &w.task,
            json!({ "field": "details", "reason": LOGGED_REASON_ALLOWED }),
        )
        .await;
    expect(&allowed, 200);
    let refused = w
        .reveal(
            &w.viewer,
            "task",
            &w.task,
            json!({ "field": "details", "reason": LOGGED_REASON_REFUSED }),
        )
        .await;
    assert_eq!(refused.status, 403, "{}", wire(&refused));

    // Both reasons are in the durable events, verbatim, and the events are valid.
    let traces = w.traces("details").await;
    for (reason, authorised) in [
        (LOGGED_REASON_ALLOWED, true),
        (LOGGED_REASON_REFUSED, false),
    ] {
        let trace = traces
            .iter()
            .find(|t| t["reason"] == reason)
            .unwrap_or_else(|| panic!("no event carries the reason {reason:?}: {traces:?}"));
        assert_eq!(trace["authorised"], json!(authorised));
        identity::validate("identity_field_accessed", trace).unwrap();
    }
    // The value is in no event.
    for (topic, payload) in w.events().await.into_iter().skip(before) {
        let text = payload.to_string();
        assert!(
            !text.contains(PLANTED),
            "the value is in the `{topic}` event"
        );
    }

    // The log names who, document, field and outcome, and holds neither reason nor value.
    let text = log.lock().unwrap().clone();
    assert!(
        text.contains("spot::audit::field_guard"),
        "the audit log line of the reveal was not captured"
    );
    for secret in [LOGGED_REASON_ALLOWED, LOGGED_REASON_REFUSED, PLANTED] {
        assert!(!text.contains(secret), "{secret:?} reached a log line");
    }
    w.started.finish().await;
}
