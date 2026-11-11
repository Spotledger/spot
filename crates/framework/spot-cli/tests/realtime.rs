//! Realtime over WebSocket against real PostgreSQL: who may join which room, what a committed
//! change tells the room, what a permission change does to a connected client (it leaves the
//! room, the connection stays), when a connection is closed (its session ends), and which
//! notices reach which user.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::notes;
use common::*;
use serde_json::json;
use spot_cli::audit::{CapturingAudit, IdentityAudit};
use std::sync::Arc;
use std::time::Duration;

const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const OTHER_PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";

/// A host started with a capturing identity audit and an admin API key; returns the key.
async fn audited_world() -> Option<(Env, Started, Arc<CapturingAudit>, String)> {
    let env = Env::new("").await?;
    let key_file = env.root.0.join("admin.key");
    ok(
        &env.root.0,
        &[
            "new-site",
            &env.site,
            "--admin-api-key-file",
            key_file.to_str().unwrap(),
        ],
    );
    let key = std::fs::read_to_string(&key_file)
        .unwrap()
        .trim()
        .to_owned();
    // Until a password is chosen the key is gated (restricted); lift the gate so a valid key works.
    env.set_password(&env.site, "administrator", VIEWER_PASSWORD);
    let sink = Arc::new(CapturingAudit::new());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let audit: Arc<dyn IdentityAudit> = sink.clone();
    let running =
        spot_cli::start_with_audit(&env.root.0, false, fast_options(), Some(audit), async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    let addr = running.local_addrs()[0];
    Some((
        env,
        Started {
            running,
            addr,
            stop,
        },
        sink,
        key,
    ))
}

/// The right shape, the right id, the wrong secret.
fn wrong_secret(key: &str) -> String {
    let (id, secret) = key.strip_prefix("spk_").unwrap().split_once('_').unwrap();
    format!("spk_{id}_{}", "A".repeat(secret.len()))
}

struct World {
    env: Env,
    started: Started,
    admin: String,
    viewer: String,
    other: String,
    project: String,
}

async fn world() -> Option<World> {
    let env = Env::new("").await?;
    let first = env.site();
    env.user("viewer@example.com", VIEWER_PASSWORD);
    env.user("other@example.com", OTHER_PASSWORD);
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
            "code": "PRJ-RT-1", "title": "Realtime", "status": "Active",
            "owner": user_id(addr, &host, &admin, "viewer@example.com").await,
            "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project = project.body["data"]["id"].as_str().unwrap().to_owned();
    let viewer = token_of(addr, &host, "viewer@example.com", VIEWER_PASSWORD).await;
    let other = token_of(addr, &host, "other@example.com", OTHER_PASSWORD).await;
    Some(World {
        env,
        started,
        admin,
        viewer,
        other,
        project,
    })
}

async fn create_task(w: &World, subject: &str) -> String {
    let reply = http(
        w.started.addr,
        &w.env.site,
        Some(&w.admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": subject, "project": w.project})),
    )
    .await;
    expect(&reply, 201);
    reply.body["data"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_hears_the_room_it_may_join_and_nothing_from_the_one_it_may_not() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());

    let mut viewer = Ws::connect(addr, &host).await;
    viewer.sign_in(&w.viewer).await;
    let joined = viewer.join("doctype:task").await;
    assert_eq!(joined["type"], "joined", "{joined}");

    let mut other = Ws::connect(addr, &host).await;
    other.sign_in(&w.other).await;
    let refused = other.join("doctype:task").await;
    assert_eq!(refused["type"], "join_error", "{refused}");

    // Another user's change: the viewer who may read tasks is told, with identifiers only.
    let task = create_task(&w, "Hear me").await;
    let event = viewer.expect_type("event", Duration::from_secs(10)).await;
    assert_eq!(event["room"], "doctype:task");
    assert_eq!(event["kind"], "doc_created");
    assert_eq!(event["payload"]["id"], task);
    assert!(
        !event.to_string().contains("Hear me"),
        "a notification never carries a field value: {event}"
    );
    // The client that was refused the room hears nothing from it.
    assert!(matches!(
        other.next(Duration::from_secs(1)).await,
        Frame::Quiet
    ));
    // A document room follows the same rule.
    let doc_room = format!("doc:task:{task}");
    assert_eq!(viewer.join(&doc_room).await["type"], "joined");
    assert_eq!(other.join(&doc_room).await["type"], "join_error");

    // A token that is not a session never gets a `ready`.
    let mut stranger = Ws::connect(addr, &host).await;
    stranger
        .send(json!({"type": "auth", "token": "not-a-session"}))
        .await;
    assert!(matches!(
        stranger.next(Duration::from_secs(5)).await,
        Frame::Close(4001)
    ));
    w.started.finish().await;
}

const VIEWER_GRANT: &str = "role = \"Viewer\"\ndoctype = \"task\"\nrights = [\"read\", \"select\"]";
const VIEWER_NO_READ: &str = "role = \"Viewer\"\ndoctype = \"task\"\nrights = [\"select\"]";

/// Rewrites the Viewer's grant on Task and publishes the new snapshot (a reload).
fn set_viewer_grant(w: &World, from: &str, to: &str) {
    let grants = w.env.root.0.join("sample/permissions/grants.toml");
    let text = std::fs::read_to_string(&grants).unwrap();
    assert!(text.contains(from), "the grant to edit is not there");
    std::fs::write(&grants, text.replace(from, to)).unwrap();
    spot_cli::request_reload(&w.env.root.0, &w.env.site).unwrap();
}

/// Waits until a fresh connection of the viewer is decided `want` (`joined` or `join_error`)
/// for the Task room: the reload is asynchronous, and the realtime hub re-asks its rooms right
/// after the snapshot is published.
async fn until_task_room_is(w: &World, want: &str) {
    for _ in 0..120 {
        let mut probe = Ws::connect(w.started.addr, &w.env.site).await;
        probe.sign_in(&w.viewer).await;
        if probe.join("doctype:task").await["type"] == want {
            // The hub's revalidation follows the publication within moments.
            tokio::time::sleep(Duration::from_secs(2)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("the snapshot never made the Task room {want} for the viewer");
}

/// Every message that arrives within `wait`; a close or a dropped connection fails the test.
async fn messages(ws: &mut Ws, wait: Duration) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return seen;
        }
        match ws.next(left).await {
            Frame::Text(message) => seen.push(message),
            Frame::Quiet => return seen,
            other => panic!("the connection was closed ({other:?}) after {seen:?}"),
        }
    }
}

fn room_events<'a>(
    seen: &'a [serde_json::Value],
    room: &'a str,
) -> impl Iterator<Item = &'a serde_json::Value> {
    seen.iter()
        .filter(move |m| m["type"] == "event" && m["room"] == room)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_edit_that_removes_read_takes_the_room_and_leaves_the_connection_open() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let viewer_id = user_id(addr, &host, &w.admin, "viewer@example.com").await;
    let own_room = format!("user:{viewer_id}");
    let mut viewer = Ws::connect(addr, &host).await;
    viewer.sign_in(&w.viewer).await;
    assert_eq!(viewer.join("doctype:task").await["type"], "joined");
    assert_eq!(viewer.join(&own_room).await["type"], "joined");

    // Before the edit the viewer hears the DocType's room.
    create_task(&w, "Before the edit").await;
    let before = messages(&mut viewer, Duration::from_secs(3)).await;
    assert_eq!(
        room_events(&before, "doctype:task").count(),
        1,
        "{before:?}"
    );

    // Take the right away and publish the snapshot: the room goes, the connection does not.
    set_viewer_grant(&w, VIEWER_GRANT, VIEWER_NO_READ);
    until_task_room_is(&w, "join_error").await;
    create_task(&w, "After the edit").await;
    let after = messages(&mut viewer, Duration::from_secs(3)).await;
    assert_eq!(
        room_events(&after, "doctype:task").count(),
        0,
        "an event of the room reached a principal who may no longer read it: {after:?}"
    );
    // The connection is alive and still in its other room, and the room is refused afresh.
    assert_eq!(viewer.join(&own_room).await["type"], "joined");
    assert_eq!(viewer.join("doctype:task").await["type"], "join_error");

    // Restore the right: the same connection re-joins and hears the room again.
    set_viewer_grant(&w, VIEWER_NO_READ, VIEWER_GRANT);
    until_task_room_is(&w, "joined").await;
    let mut rejoined = false;
    for _ in 0..20 {
        if viewer.join("doctype:task").await["type"] == "joined" {
            rejoined = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        rejoined,
        "the restored right did not let the client re-join"
    );
    create_task(&w, "After the restore").await;
    let restored = messages(&mut viewer, Duration::from_secs(5)).await;
    assert_eq!(
        room_events(&restored, "doctype:task").count(),
        1,
        "{restored:?}"
    );
    w.started.finish().await;
}

/// The viewer's own role assignment: its id and the ETag a delete must carry.
async fn viewer_assignment(w: &World) -> (String, String) {
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let viewer_id = user_id(addr, &host, &w.admin, "viewer@example.com").await;
    let list = http(
        addr,
        &host,
        Some(&w.admin),
        "GET",
        "/api/v1/resource/role_assignment?limit=100",
        &[],
        None,
    )
    .await;
    expect(&list, 200);
    let id = list.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["user"] == viewer_id)
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .expect("the viewer has a role assignment");
    let one = http(
        addr,
        &host,
        Some(&w.admin),
        "GET",
        &format!("/api/v1/resource/role_assignment/{id}"),
        &[],
        None,
    )
    .await;
    expect(&one, 200);
    (id, one.etag)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_role_assignment_takes_the_room_at_the_next_pump_run_not_at_the_timer() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let (assignment, etag) = viewer_assignment(&w).await;
    let viewer_id = user_id(addr, &host, &w.admin, "viewer@example.com").await;
    let own_room = format!("user:{viewer_id}");
    let mut viewer = Ws::connect(addr, &host).await;
    viewer.sign_in(&w.viewer).await;
    assert_eq!(viewer.join("doctype:task").await["type"], "joined");
    assert_eq!(viewer.join(&own_room).await["type"], "joined");
    create_task(&w, "Before the removal").await;
    let before = messages(&mut viewer, Duration::from_secs(2)).await;
    assert_eq!(
        room_events(&before, "doctype:task").count(),
        1,
        "{before:?}"
    );

    // An operator's connection hears the pump feed the removal, which tells the test when the
    // pump has reached it however loaded the machine is.
    let mut operator = Ws::connect(addr, &host).await;
    operator.sign_in(&w.admin).await;
    assert_eq!(
        operator.join("doctype:role_assignment").await["type"],
        "joined"
    );

    // Remove the grant. It is a stored record: no snapshot is published.
    let deleted = http(
        addr,
        &host,
        Some(&w.admin),
        "DELETE",
        &format!("/api/v1/resource/role_assignment/{assignment}"),
        &[("If-Match", &etag)],
        None,
    )
    .await;
    assert!(
        deleted.status == 200 || deleted.status == 204,
        "{}",
        deleted.body
    );
    let fed = operator.expect_type("event", Duration::from_secs(10)).await;
    assert_eq!(fed["payload"]["id"], assignment, "{fed}");
    let fed_at = tokio::time::Instant::now();

    // The pump polls every 100 ms and revalidates right after the run that fed the removal; the
    // 10 s timer must not be what notices. A later task would be heard by the connection if the
    // room were still joined.
    tokio::time::sleep(Duration::from_millis(600)).await;
    create_task(&w, "After the removal").await;
    let after = messages(&mut viewer, Duration::from_millis(1200)).await;
    assert!(
        fed_at.elapsed() < Duration::from_secs(5),
        "the checks took too long to prove anything"
    );
    assert_eq!(
        room_events(&after, "doctype:task").count(),
        0,
        "a principal whose role assignment was removed still hears the room: {after:?}"
    );
    // The connection is alive and still in its own room; the Task room is refused afresh.
    assert_eq!(viewer.join(&own_room).await["type"], "joined");
    assert_eq!(viewer.join("doctype:task").await["type"], "join_error");
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrelated_document_change_leaves_the_joined_rooms_alone() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let mut viewer = Ws::connect(addr, &host).await;
    viewer.sign_in(&w.viewer).await;
    assert_eq!(viewer.join("doctype:task").await["type"], "joined");
    for n in 0..3 {
        create_task(&w, &format!("Unrelated {n}")).await;
    }
    // Every change is heard, nothing is evicted and the connection stays open.
    let seen = messages(&mut viewer, Duration::from_secs(2)).await;
    assert_eq!(room_events(&seen, "doctype:task").count(), 3, "{seen:?}");
    assert_eq!(viewer.join("doctype:task").await["type"], "joined");
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_session_still_closes_the_connection_with_4009() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let mut viewer = Ws::connect(addr, &host).await;
    viewer.sign_in(&w.viewer).await;
    assert_eq!(viewer.join("doctype:task").await["type"], "joined");
    let other_session = {
        // A second connection on a different session is not affected.
        let mut other = Ws::connect(addr, &host).await;
        other.sign_in(&w.other).await;
        other
    };

    expect(
        &http(
            addr,
            &host,
            Some(&w.viewer),
            "POST",
            "/api/v1/auth/logout",
            &[],
            None,
        )
        .await,
        204,
    );
    // The transport asks the sessions again every few seconds.
    let mut closed = None;
    for _ in 0..40 {
        match viewer.next(Duration::from_secs(1)).await {
            Frame::Close(code) => {
                closed = Some(code);
                break;
            }
            Frame::Gone => break,
            Frame::Text(_) | Frame::Quiet => {}
        }
    }
    assert_eq!(closed, Some(4009), "a revoked session must close with 4009");
    drop(other_session);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notice_reaches_its_user_after_commit_and_never_another_or_a_rolled_back_one() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    env.user("viewer@example.com", VIEWER_PASSWORD);
    env.user("other@example.com", OTHER_PASSWORD);
    notes::install(&env, &[&env.site]).await;
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let other_id = user_id(addr, &host, &admin, "other@example.com").await;
    let viewer = token_of(addr, &host, "viewer@example.com", VIEWER_PASSWORD).await;
    let other = token_of(addr, &host, "other@example.com", OTHER_PASSWORD).await;

    let mut viewer_ws = Ws::connect(addr, &host).await;
    viewer_ws.sign_in(&viewer).await;
    assert_eq!(
        viewer_ws.join(&format!("user:{viewer_id}")).await["type"],
        "joined"
    );
    let mut other_ws = Ws::connect(addr, &host).await;
    other_ws.sign_in(&other).await;
    assert_eq!(
        other_ws.join(&format!("user:{other_id}")).await["type"],
        "joined"
    );
    // Another user's notification room is refused.
    assert_eq!(
        other_ws.join(&format!("user:{viewer_id}")).await["type"],
        "join_error"
    );

    // A committed note whose hook called `notify_user(viewer)`.
    let made = notes::make(addr, &host, &admin, &format!("notify:{viewer_id}")).await;
    expect(&made, 201);
    let notice = viewer_ws
        .expect_type("event", Duration::from_secs(10))
        .await;
    assert_eq!(notice["room"], format!("user:{viewer_id}"));
    assert_eq!(notice["kind"], "notification");
    assert_eq!(notice["payload"]["event"], "user_notice");
    assert_eq!(notice["payload"]["data"]["kind"], "note_created");
    assert!(
        !notice.to_string().contains(&format!("notify:{viewer_id}")),
        "the notice carries ids and short scalars only: {notice}"
    );
    // Nothing for the other user.
    assert!(
        messages(&mut other_ws, Duration::from_secs(2))
            .await
            .is_empty(),
        "another user heard the notice"
    );

    // The hook called `notify_user` and then the transaction failed: nothing is delivered.
    let refused = notes::make(addr, &host, &admin, &format!("notify:{viewer_id}:boom")).await;
    assert!(refused.status >= 400, "the note should have been refused");
    assert!(
        messages(&mut viewer_ws, Duration::from_secs(4))
            .await
            .is_empty(),
        "a notice of a rolled-back transaction was delivered"
    );
    started.finish().await;
}

/// WP-PRD-01.T46: a refused API key on the realtime socket is audited once with the path class
/// `realtime`; a valid key leaves no record and signs in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_api_key_on_the_realtime_socket_is_audited_as_realtime() {
    let Some((env, started, sink, key)) = audited_world().await else {
        return;
    };
    let (addr, host) = (started.addr, env.site.clone());

    let mut good = Ws::connect(addr, &host).await;
    good.sign_in(&key).await;
    assert!(
        sink.of("api_key_rejected").is_empty(),
        "{:?}",
        sink.records()
    );

    let wrong = wrong_secret(&key);
    let mut bad = Ws::connect(addr, &host).await;
    bad.send(json!({"type": "auth", "token": wrong})).await;
    // Whatever the socket answers, it is never `ready`.
    while let Frame::Text(m) = bad.next(Duration::from_secs(5)).await {
        assert_ne!(m["type"], "ready", "{m}");
    }
    let rejected = sink.of("api_key_rejected");
    assert_eq!(rejected.len(), 1, "{:?}", sink.records());
    assert_eq!(rejected[0].path.as_deref(), Some("realtime"));
    let text = serde_json::to_string(&sink.records()).unwrap();
    for needle in [wrong.as_str(), key.as_str()] {
        assert!(!text.contains(needle), "leaked: {text}");
    }
    started.finish().await;
}
