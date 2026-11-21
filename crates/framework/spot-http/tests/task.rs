//! The task inbox of native API 0.11.0 (WP-BPM-05.T7): the seven operations over the wire,
//! through a [`MemoryTaskPort`]: rights, the version check, idempotency, segregation, the
//! existence oracle, bounds, site isolation and the unconfigured service.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "task/support.rs"]
mod support;

use axum::http::header;
use serde_json::{Value as Json, json};
use spot_http::api::router::routes::task;
use support::{
    HOST, OTHER_HOST, code, etag, fixture, ids, post_with, send, send_on, stripped, unconfigured,
};

#[tokio::test]
async fn the_happy_path_of_each_operation() {
    let f = fixture();
    // task_list: alice's own, with the contract's page shape.
    let list = send(&f, "GET", "/api/v1/task", Some("tok-alice"), &[], None).await;
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(ids(&list), ["t_one", "t_two"]);
    assert_eq!(list.body["next_cursor"], Json::Null);
    assert_eq!(list.body["data"][0]["state"], "open");
    assert_eq!(
        list.headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    // task_get: ETag, outcomes, fields and events.
    let got = send(
        &f,
        "GET",
        "/api/v1/task/t_one",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(got.status, 200, "{}", got.body);
    assert_eq!(etag(&got), "\"1\"");
    assert_eq!(got.body["data"]["task"]["id"], "t_one");
    assert_eq!(got.body["data"]["outcomes"][0]["outcome"], "approve");
    assert_eq!(got.body["data"]["events"][0]["kind"], "created");
    // task_claim, task_release.
    let claimed = post_with(&f, "/api/v1/task/t_one/claim", "tok-alice", "1", None, None).await;
    assert_eq!(claimed.status, 200, "{}", claimed.body);
    assert_eq!(etag(&claimed), "\"2\"");
    assert_eq!(claimed.body["data"]["state"], "claimed");
    assert_eq!(claimed.body["data"]["claimed_by"], "alice");
    let released = post_with(
        &f,
        "/api/v1/task/t_one/release",
        "tok-alice",
        "2",
        None,
        None,
    )
    .await;
    assert_eq!(released.status, 200, "{}", released.body);
    assert_eq!(etag(&released), "\"3\"");
    assert_eq!(released.body["data"]["state"], "open");
    assert!(released.body["data"].get("claimed_by").is_none());
    // task_complete.
    let done = post_with(
        &f,
        "/api/v1/task/t_one/complete",
        "tok-alice",
        "3",
        Some("key-1"),
        Some(json!({"outcome": "approve", "comment": "Looks right."})),
    )
    .await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.body["data"]["state"], "completed");
    assert_eq!(etag(&done), "\"4\"");
    // task_delegate: carol can then see and act on t_two.
    let before = send(&f, "GET", "/api/v1/task", Some("tok-carol"), &[], None).await;
    assert!(ids(&before).is_empty());
    let delegated = post_with(
        &f,
        "/api/v1/task/t_two/delegate",
        "tok-alice",
        "1",
        None,
        Some(json!({"to": "carol", "until": "2026-12-01T00:00:00Z", "reason": "On leave."})),
    )
    .await;
    assert_eq!(delegated.status, 200, "{}", delegated.body);
    assert_eq!(etag(&delegated), "\"2\"");
    let after = send(&f, "GET", "/api/v1/task", Some("tok-carol"), &[], None).await;
    assert_eq!(ids(&after), ["t_two"]);
    // task_reassign: the supervisor moves it to bob, who is then the only assignee.
    let reassigned = post_with(
        &f,
        "/api/v1/task/t_two/reassign",
        "tok-mgr",
        "2",
        None,
        Some(json!({"to": "bob", "reason": "Covering the queue."})),
    )
    .await;
    assert_eq!(reassigned.status, 200, "{}", reassigned.body);
    assert_eq!(reassigned.body["data"]["assignees"], json!(["bob"]));
    assert_eq!(etag(&reassigned), "\"3\"");
    let gone = send(
        &f,
        "GET",
        "/api/v1/task/t_two",
        Some("tok-carol"),
        &[],
        None,
    )
    .await;
    assert_eq!(gone.status, 404);
}

#[tokio::test]
async fn a_double_complete_replays_with_the_same_key_and_is_not_active_with_another() {
    let f = fixture();
    let body = json!({"outcome": "approve"});
    let first = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("same-key"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert!(first.headers.get("idempotency-replayed").is_none());
    let replay = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("same-key"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(replay.status, 200, "{}", replay.body);
    assert_eq!(replay.headers.get("idempotency-replayed").unwrap(), "true");
    assert_eq!(replay.body, first.body);
    // The same key for another outcome is the conflict.
    let conflict = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("same-key"),
        Some(json!({"outcome": "reject"})),
    )
    .await;
    assert_eq!(conflict.status, 409);
    assert_eq!(code(&conflict), "api.idempotency_conflict");
    // A new key on the finished task is not_active.
    let other = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("another-key"),
        Some(body),
    )
    .await;
    assert_eq!(other.status, 409);
    assert_eq!(code(&other), "task.not_active");
    assert_eq!(f.port.task("acme", "t_two").unwrap().version, 2);
}

#[tokio::test]
async fn a_stale_version_is_412_and_changes_nothing() {
    let f = fixture();
    for (path, body) in [
        ("/api/v1/task/t_two/claim", None),
        ("/api/v1/task/t_two/release", None),
        ("/api/v1/task/t_two/delegate", Some(json!({"to": "carol"}))),
    ] {
        let r = post_with(&f, path, "tok-alice", "9", None, body).await;
        assert_eq!(r.status, 412, "{path}: {}", r.body);
        assert_eq!(code(&r), "document.stale_version", "{path}");
        assert_eq!(r.body["error"]["details"]["current_version"], 1, "{path}");
    }
    let r = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "9",
        Some("k"),
        Some(json!({"outcome": "approve"})),
    )
    .await;
    assert_eq!(r.status, 412);
    let r = post_with(
        &f,
        "/api/v1/task/t_two/reassign",
        "tok-mgr",
        "9",
        None,
        Some(json!({"to": "bob"})),
    )
    .await;
    assert_eq!(r.status, 412);
    assert_eq!(f.port.task("acme", "t_two").unwrap().version, 1);
}

#[tokio::test]
async fn if_match_is_required_on_every_change_and_the_key_on_complete() {
    let f = fixture();
    let key = [("idempotency-key", "k")];
    for (path, token, extra, body) in [
        ("/api/v1/task/t_two/claim", "tok-alice", &[][..], None),
        ("/api/v1/task/t_two/release", "tok-alice", &[][..], None),
        (
            "/api/v1/task/t_two/complete",
            "tok-alice",
            &key[..],
            Some(json!({"outcome": "approve"})),
        ),
        (
            "/api/v1/task/t_two/delegate",
            "tok-alice",
            &[][..],
            Some(json!({"to": "carol"})),
        ),
        (
            "/api/v1/task/t_two/reassign",
            "tok-mgr",
            &[][..],
            Some(json!({"to": "bob"})),
        ),
    ] {
        let r = send(&f, "POST", path, Some(token), extra, body).await;
        assert_eq!(r.status, 428, "{path}: {}", r.body);
        assert_eq!(code(&r), "api.precondition_required", "{path}");
    }
    // A malformed If-Match is a malformed request, not a pass.
    let r = send(
        &f,
        "POST",
        "/api/v1/task/t_two/claim",
        Some("tok-alice"),
        &[("if-match", "1")],
        None,
    )
    .await;
    assert_eq!(r.status, 400);
    // A complete without its Idempotency-Key is refused before the port is asked.
    for key in [None, Some(""), Some("has space")] {
        let r = post_with(
            &f,
            "/api/v1/task/t_two/complete",
            "tok-alice",
            "1",
            key,
            Some(json!({"outcome": "approve"})),
        )
        .await;
        assert_eq!(r.status, 400, "{key:?}: {}", r.body);
        assert_eq!(code(&r), "api.invalid_request");
    }
    assert_eq!(f.port.task("acme", "t_two").unwrap().version, 1);
    assert!(f.port.audit().is_empty(), "no refusal reached the port");
}

#[tokio::test]
async fn a_body_naming_an_actor_is_rejected_and_nothing_changes() {
    let f = fixture();
    for (path, token, body) in [
        (
            "/api/v1/task/t_two/complete",
            "tok-alice",
            json!({"outcome": "approve", "actor": "system:sla"}),
        ),
        (
            "/api/v1/task/t_two/complete",
            "tok-alice",
            json!({"outcome": "approve", "completed_by": "mgr"}),
        ),
        (
            "/api/v1/task/t_two/delegate",
            "tok-alice",
            json!({"to": "carol", "actor": "mgr"}),
        ),
        (
            "/api/v1/task/t_two/reassign",
            "tok-mgr",
            json!({"to": "bob", "actor": "alice"}),
        ),
    ] {
        let r = post_with(&f, path, token, "1", Some("k"), Some(body)).await;
        assert_eq!(r.status, 400, "{path}: {}", r.body);
        assert_eq!(code(&r), "api.invalid_request", "{path}");
        let text = r.body.to_string();
        assert!(!text.contains("system:sla") && !text.contains("completed_by"));
    }
    // A query parameter naming an actor is an unknown parameter too.
    let r = send(
        &f,
        "GET",
        "/api/v1/task?user=bob",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(f.port.task("acme", "t_two").unwrap().version, 1);
}

#[tokio::test]
async fn a_stranger_gets_byte_identical_404s_for_an_unknown_task() {
    let f = fixture();
    let complete = Some(json!({"outcome": "approve"}));
    let cases: [(&str, &str, bool, Option<Json>); 3] = [
        ("GET", "", false, None),
        ("POST", "/claim", false, None),
        ("POST", "/complete", true, complete),
    ];
    for (method, suffix, keyed, body) in cases {
        let mut replies = Vec::new();
        for id in ["t_one", "t_nothing"] {
            let path = format!("/api/v1/task/{id}{suffix}");
            let mut extra = vec![("if-match", "\"1\"")];
            if keyed {
                extra.push(("idempotency-key", "k"));
            }
            let extra: &[(&str, &str)] = if method == "GET" { &[] } else { &extra };
            replies.push(send(&f, method, &path, Some("tok-bob"), extra, body.clone()).await);
        }
        assert_eq!(replies[0].status, 404, "{method} {suffix}");
        assert_eq!(code(&replies[0]), "task.not_found");
        assert_eq!(
            stripped(&replies[0]),
            stripped(&replies[1]),
            "{method} {suffix}: a task the caller may not see is an unknown task"
        );
    }
    // Nothing was audited for the stranger, and the task is untouched.
    assert!(f.port.audit().is_empty());
    assert_eq!(f.port.task("acme", "t_one").unwrap().version, 1);
}

#[tokio::test]
async fn a_segregation_refusal_is_403_and_leaves_the_task_unchanged() {
    let f = fixture();
    let before = f.port.task("acme", "t_one").unwrap();
    let r = post_with(
        &f,
        "/api/v1/task/t_one/complete",
        "tok-dave",
        "1",
        Some("k"),
        Some(json!({"outcome": "approve"})),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(code(&r), "task.segregation");
    assert_eq!(f.port.task("acme", "t_one").unwrap(), before);
    // The refusal is audited: who, which task, which rule.
    let audit = f.port.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        (
            audit[0].user.as_str(),
            audit[0].task.as_str(),
            audit[0].result
        ),
        ("dave", "t_one", "task.segregation")
    );
}

#[tokio::test]
async fn the_refusals_of_each_operation_use_the_declared_codes() {
    let f = fixture();
    // alice claims t_one; dave then cannot claim, and cannot release what he does not hold.
    let c = post_with(&f, "/api/v1/task/t_one/claim", "tok-alice", "1", None, None).await;
    assert_eq!(c.status, 200);
    let r = post_with(&f, "/api/v1/task/t_one/claim", "tok-dave", "2", None, None).await;
    assert_eq!((r.status, code(&r)), (409, "task.already_claimed"));
    let r = post_with(
        &f,
        "/api/v1/task/t_one/release",
        "tok-dave",
        "2",
        None,
        None,
    )
    .await;
    assert_eq!((r.status, code(&r)), (409, "task.not_claimed_by_caller"));
    // The holder repeating the claim is a success with no change.
    let r = post_with(&f, "/api/v1/task/t_one/claim", "tok-alice", "2", None, None).await;
    assert_eq!((r.status, etag(&r).as_str()), (200, "\"2\""));
    // A supervisor may release anyone's claim.
    let r = post_with(&f, "/api/v1/task/t_one/release", "tok-mgr", "2", None, None).await;
    assert_eq!(r.status, 200, "{}", r.body);
    // The supervisor who cannot act may neither claim nor complete: permission.denied.
    let r = post_with(&f, "/api/v1/task/t_two/claim", "tok-mgr", "1", None, None).await;
    assert_eq!((r.status, code(&r)), (403, "permission.denied"));
    // Reassign is for supervisors only.
    let r = post_with(
        &f,
        "/api/v1/task/t_two/reassign",
        "tok-alice",
        "1",
        None,
        Some(json!({"to": "bob"})),
    )
    .await;
    assert_eq!((r.status, code(&r)), (403, "permission.denied"));
    // An outcome the task does not offer; the answer names no submitted value.
    let r = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("k"),
        Some(json!({"outcome": "secret-outcome-xyz"})),
    )
    .await;
    assert_eq!((r.status, code(&r)), (422, "task.outcome_invalid"));
    assert!(!r.body.to_string().contains("secret-outcome-xyz"));
    // Targets: unknown and self are the same refusal; an end time in the past differs.
    for to in ["nobody", "alice"] {
        let r = post_with(
            &f,
            "/api/v1/task/t_two/delegate",
            "tok-alice",
            "1",
            None,
            Some(json!({"to": to})),
        )
        .await;
        assert_eq!((r.status, code(&r)), (422, "task.target_invalid"), "{to}");
    }
    let r = post_with(
        &f,
        "/api/v1/task/t_two/reassign",
        "tok-mgr",
        "1",
        None,
        Some(json!({"to": "mgr"})),
    )
    .await;
    assert_eq!((r.status, code(&r)), (422, "task.target_invalid"));
    let r = post_with(
        &f,
        "/api/v1/task/t_two/delegate",
        "tok-alice",
        "1",
        None,
        Some(json!({"to": "carol", "until": "2026-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!((r.status, code(&r)), (422, "task.delegation_invalid"));
    let r = post_with(
        &f,
        "/api/v1/task/t_two/delegate",
        "tok-alice",
        "1",
        None,
        Some(json!({"to": "carol", "until": "tomorrow"})),
    )
    .await;
    assert_eq!((r.status, code(&r)), (400, "api.invalid_request"));
    assert_eq!(f.port.task("acme", "t_two").unwrap().version, 1);
}

#[tokio::test]
async fn the_views_are_filtered_by_rights_before_they_are_paged() {
    let f = fixture();
    // view=team for a user who supervises nobody is an empty page, not an error.
    let r = send(
        &f,
        "GET",
        "/api/v1/task?view=team",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, json!({"data": [], "next_cursor": null}));
    // The supervisor's team view holds both; she acts on neither.
    let r = send(
        &f,
        "GET",
        "/api/v1/task?view=team",
        Some("tok-mgr"),
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&r), ["t_one", "t_two"]);
    let r = send(&f, "GET", "/api/v1/task", Some("tok-mgr"), &[], None).await;
    assert!(ids(&r).is_empty());
    // overdue is `mine` past the due time; process narrows.
    let r = send(
        &f,
        "GET",
        "/api/v1/task?view=overdue",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&r), ["t_one"]);
    let r = send(
        &f,
        "GET",
        "/api/v1/task?process=leave",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&r), ["t_two"]);
    // Paging by the opaque cursor.
    let first = send(
        &f,
        "GET",
        "/api/v1/task?limit=1",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&first), ["t_one"]);
    let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
    let second = send(
        &f,
        "GET",
        &format!("/api/v1/task?limit=1&cursor={cursor}"),
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&second), ["t_two"]);
    assert_eq!(second.body["next_cursor"], Json::Null);
    // Malformed queries.
    for q in [
        "view=all",
        "view=mine&view=team",
        "limit=0",
        "limit=101",
        "limit=x",
        "process=Bad-Name",
        "cursor=",
        "cursor=forged",
        "page=2",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/task?{q}"),
            Some("tok-alice"),
            &[],
            None,
        )
        .await;
        assert_eq!(r.status, 400, "{q}: {}", r.body);
        assert_eq!(code(&r), "api.invalid_request", "{q}");
    }
}

#[tokio::test]
async fn the_values_of_a_completion_are_bounded() {
    let f = fixture();
    let ok = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("k-ok"),
        Some(json!({"outcome": "approve", "values": {"amount": "10.50", "lines": [{"a": 1}]}})),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    let f = fixture();
    let mut deep = json!(1);
    for _ in 0..10 {
        deep = json!({ "k": deep });
    }
    let many: serde_json::Map<String, Json> =
        (0..101).map(|i| (format!("f{i}"), json!(i))).collect();
    let cases = [
        json!({"outcome": "approve", "values": {"a": deep}}),
        json!({"outcome": "approve", "values": many}),
        json!({"outcome": "approve", "values": {"a": "x".repeat(70_000)}}),
        json!({"outcome": "approve", "values": {"Bad Name": 1}}),
        json!({"outcome": "approve", "values": [1, 2]}),
        json!({"outcome": "approve", "comment": "c".repeat(2001)}),
        json!({"outcome": ""}),
        json!({"comment": "no outcome"}),
        json!(["approve"]),
    ];
    for (n, body) in cases.into_iter().enumerate() {
        let r = post_with(
            &f,
            "/api/v1/task/t_two/complete",
            "tok-alice",
            "1",
            Some("k"),
            Some(body),
        )
        .await;
        assert!(
            matches!(r.status, 400 | 413),
            "case {n}: {} {}",
            r.status,
            r.body
        );
    }
    assert_eq!(f.port.task("acme", "t_two").unwrap().version, 1);
    // An oversized body is refused by its declared length, before it is buffered.
    let huge = json!({"outcome": "approve", "values": {"a": "x".repeat(200_000)}});
    let r = post_with(
        &f,
        "/api/v1/task/t_two/complete",
        "tok-alice",
        "1",
        Some("k"),
        Some(huge),
    )
    .await;
    assert_eq!(r.status, 413);
    assert_eq!(code(&r), "http.body_too_large");
}

#[tokio::test]
async fn a_path_id_that_is_not_the_documented_form_is_not_passed_on() {
    let f = fixture();
    let long = "a".repeat(65);
    for id in ["T_ONE", "1abc", "t-one", long.as_str()] {
        let path = format!("/api/v1/task/{id}");
        let r = send(&f, "GET", &path, Some("tok-alice"), &[], None).await;
        assert_eq!(r.status, 400, "{id}");
        assert_eq!(code(&r), "api.invalid_request", "{id}");
        let r = post_with(&f, &format!("{path}/claim"), "tok-alice", "1", None, None).await;
        assert_eq!(r.status, 400, "{id}");
    }
    assert!(f.port.audit().is_empty());
}

#[tokio::test]
async fn two_sites_share_nothing() {
    let f = fixture();
    // Beta's alice sees beta's task only; acme's alice sees acme's only.
    let beta = send_on(
        &f.router,
        OTHER_HOST,
        "GET",
        "/api/v1/task",
        Some("tok-beta"),
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&beta), ["t_beta"]);
    let acme = send(&f, "GET", "/api/v1/task", Some("tok-alice"), &[], None).await;
    assert!(!ids(&acme).contains(&"t_beta".to_owned()));
    // Site A's token reads nothing of site B: unknown there, and site B's task is a 404 here.
    let wrong = send_on(
        &f.router,
        OTHER_HOST,
        "GET",
        "/api/v1/task",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(wrong.status, 401);
    let cross = send(
        &f,
        "GET",
        "/api/v1/task/t_beta",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(cross.status, 404);
    assert_eq!(code(&cross), "task.not_found");
    let cross = post_with(
        &f,
        "/api/v1/task/t_beta/claim",
        "tok-alice",
        "1",
        None,
        None,
    )
    .await;
    assert_eq!(cross.status, 404);
    let theirs = send_on(
        &f.router,
        OTHER_HOST,
        "GET",
        "/api/v1/task/t_one",
        Some("tok-beta"),
        &[],
        None,
    )
    .await;
    assert_eq!(theirs.status, 404);
    assert_eq!(f.port.task("beta", "t_beta").unwrap().version, 1);
    // A tenant named in a header or a query parameter changes nothing.
    let spoof = send(
        &f,
        "GET",
        "/api/v1/task/t_beta",
        Some("tok-alice"),
        &[("x-site", "beta"), ("host-override", OTHER_HOST)],
        None,
    )
    .await;
    assert_eq!(spoof.status, 404);
}

#[tokio::test]
async fn without_a_credential_nothing_is_served() {
    let f = fixture();
    let r = send(&f, "GET", "/api/v1/task", None, &[], None).await;
    assert_eq!(r.status, 401);
    assert_eq!(code(&r), "auth.required");
    let r = send(&f, "GET", "/api/v1/task", Some("tok-forged"), &[], None).await;
    assert_eq!(r.status, 401);
    assert!(r.body.get("data").is_none());
    let r = send(
        &f,
        "POST",
        "/api/v1/task/t_two/claim",
        None,
        &[("if-match", "\"1\"")],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn with_no_port_every_operation_is_501_service_not_configured() {
    let router = unconfigured();
    let key = [("if-match", "\"1\""), ("idempotency-key", "k")];
    let calls: [(&str, &str, Option<Json>); 7] = [
        ("GET", "/api/v1/task", None),
        ("GET", "/api/v1/task/t_one", None),
        ("POST", "/api/v1/task/t_one/claim", None),
        ("POST", "/api/v1/task/t_one/release", None),
        (
            "POST",
            "/api/v1/task/t_one/complete",
            Some(json!({"outcome": "approve"})),
        ),
        (
            "POST",
            "/api/v1/task/t_one/delegate",
            Some(json!({"to": "carol"})),
        ),
        (
            "POST",
            "/api/v1/task/t_one/reassign",
            Some(json!({"to": "carol"})),
        ),
    ];
    for (method, path, body) in calls {
        let r = send_on(&router, HOST, method, path, Some("tok-alice"), &key, body).await;
        assert_eq!(r.status, 501, "{path}: {}", r.body);
        assert_eq!(code(&r), "service.not_configured", "{path}");
        assert!(r.body.get("data").is_none(), "{path}");
    }
}

#[tokio::test]
async fn every_answer_is_private_and_carries_a_request_id() {
    let f = fixture();
    let replies = [
        send(&f, "GET", "/api/v1/task", Some("tok-alice"), &[], None).await,
        send(
            &f,
            "GET",
            "/api/v1/task/t_one",
            Some("tok-alice"),
            &[],
            None,
        )
        .await,
        send(
            &f,
            "GET",
            "/api/v1/task/t_nothing",
            Some("tok-alice"),
            &[],
            None,
        )
        .await,
        post_with(&f, "/api/v1/task/t_one/claim", "tok-alice", "9", None, None).await,
    ];
    for r in replies {
        assert_eq!(
            r.headers.get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        assert!(r.headers.get("x-request-id").is_some());
    }
}

#[tokio::test]
async fn the_registry_knows_the_seven_operations() {
    let serves: Vec<&str> = task::SERVES.to_vec();
    assert_eq!(
        serves,
        [
            "task_list",
            "task_get",
            "task_claim",
            "task_release",
            "task_complete",
            "task_delegate",
            "task_reassign"
        ]
    );
    let served = spot_http::api::served_operations();
    for op in serves {
        assert!(served.contains(op), "{op}");
    }
    assert_eq!(task::required_ports().len(), 1);
}
