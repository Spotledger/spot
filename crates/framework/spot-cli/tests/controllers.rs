//! The installed apps' controllers over real HTTP and PostgreSQL: a job enqueued in a hook
//! follows its transaction, named app permissions are decided per site from the roles' grants,
//! and the sample app's Time Entry roll-up works end to end.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::notes;
use common::*;
use rust_decimal::Decimal;
use serde_json::json;
use std::time::Duration;

const APPROVER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const READER_PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";

async fn until<F, Fut>(seconds: u64, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn make_note(addr: std::net::SocketAddr, host: &str, token: &str, title: &str) -> Reply {
    http(
        addr,
        host,
        Some(token),
        "POST",
        "/api/v1/resource/note",
        &[],
        Some(json!({ "title": title })),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_enqueued_in_a_hook_runs_once_after_commit_and_not_after_rollback() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    notes::install(&env, &[&env.site]).await;
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;

    expect(&make_note(addr, &host, &admin, "committed-note").await, 201);
    assert!(
        until(15, || async {
            notes::pings().contains(&"committed-note".to_owned())
        })
        .await,
        "the job of a committed note never ran"
    );

    // The step enqueued its job and then failed: the transaction rolled back, and so did the
    // job (it is an outbox row of the same transaction).
    let refused = make_note(addr, &host, &admin, "boom").await;
    assert!(refused.status >= 400, "the note should have been refused");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pings = notes::pings();
    assert!(!pings.contains(&"boom".to_owned()), "{pings:?}");
    // And the committed one ran exactly once.
    assert_eq!(
        pings
            .iter()
            .filter(|t| t.as_str() == "committed-note")
            .count(),
        1,
        "{pings:?}"
    );
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_method_requiring_a_named_permission_follows_the_roles_grants_per_site() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    let site_b = format!(
        "spot-test-b-{}",
        spot_storage_contract::RecordId::new_v7()
            .to_string()
            .replace('-', "")
    );
    let first_b = secret(&ok(&env.root.0, &["new-site", &site_b]));
    ok(
        &env.root.0,
        &["add-user", &env.site, "approver@example.com"],
    );
    ok(&env.root.0, &["add-user", &env.site, "reader@example.com"]);
    env.set_password(&env.site, "approver@example.com", APPROVER_PASSWORD);
    env.set_password(&env.site, "reader@example.com", READER_PASSWORD);
    ok(&env.root.0, &["add-user", &site_b, "approver@example.com"]);
    env.set_password(&site_b, "approver@example.com", APPROVER_PASSWORD);
    notes::install(&env, &[&env.site, &site_b]).await;
    let started = start(&env).await;
    let addr = started.addr;
    let host = env.site.clone();

    let admin_a = admin(addr, &host, &first).await;
    let approver_id = user_id(addr, &host, &admin_a, "approver@example.com").await;
    let reader_id = user_id(addr, &host, &admin_a, "reader@example.com").await;
    let approver_role = role(addr, &host, &admin_a, "Note Approver").await;
    let reader_role = role(addr, &host, &admin_a, "Note Reader").await;
    assign(addr, &host, &admin_a, &approver_id, &approver_role).await;
    assign(addr, &host, &admin_a, &reader_id, &reader_role).await;
    let note = make_note(addr, &host, &admin_a, "to approve").await;
    expect(&note, 201);
    let path = format!(
        "/api/v1/resource/note/{}/methods/approve",
        note.body["data"]["id"].as_str().unwrap()
    );
    let approver = token_of(addr, &host, "approver@example.com", APPROVER_PASSWORD).await;
    let reader = token_of(addr, &host, "reader@example.com", READER_PASSWORD).await;

    // A role that holds the permission runs the method; one that does not is refused.
    expect(
        &http(addr, &host, Some(&approver), "POST", &path, &[], None).await,
        200,
    );
    let refused = http(addr, &host, Some(&reader), "POST", &path, &[], None).await;
    expect(&refused, 403);
    assert_eq!(refused.body["error"]["code"], "permission.denied");

    // The role edit publishes a new snapshot: the same caller is refused after it.
    let grants = env.root.0.join("notes/permissions/app_permissions.toml");
    let granted = std::fs::read_to_string(&grants).unwrap();
    std::fs::write(&grants, "entries = []\n").unwrap();
    spot_cli::request_reload(&env.root.0, &host).unwrap();
    assert!(
        until(15, || async {
            http(addr, &host, Some(&approver), "POST", &path, &[], None)
                .await
                .status
                == 403
        })
        .await,
        "the revoked grant was still honoured after the reload"
    );
    std::fs::write(&grants, granted).unwrap();
    spot_cli::request_reload(&env.root.0, &host).unwrap();
    assert!(
        until(15, || async {
            http(addr, &host, Some(&approver), "POST", &path, &[], None)
                .await
                .status
                == 200
        })
        .await,
        "the restored grant was not honoured after the reload"
    );

    // Site B has its own snapshot: a user there with no role is refused the same method on
    // site B's own note, while site A's approver keeps theirs.
    let admin_b = admin(addr, &site_b, &first_b).await;
    let note_b = make_note(addr, &site_b, &admin_b, "to approve in b").await;
    expect(&note_b, 201);
    let approver_b = token_of(addr, &site_b, "approver@example.com", APPROVER_PASSWORD).await;
    let path_b = format!(
        "/api/v1/resource/note/{}/methods/approve",
        note_b.body["data"]["id"].as_str().unwrap()
    );
    let in_b = http(addr, &site_b, Some(&approver_b), "POST", &path_b, &[], None).await;
    assert!(in_b.status == 403 || in_b.status == 404, "{}", in_b.status);
    expect(
        &http(addr, &host, Some(&approver), "POST", &path, &[], None).await,
        200,
    );
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_method_naming_an_undeclared_permission_degrades_the_site_and_is_refused() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    notes::install(&env, &[&env.site]).await;
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;

    let ready = http(addr, &host, None, "GET", "/api/health/ready", &[], None).await;
    let text = String::from_utf8_lossy(&ready.bytes).into_owned();
    assert_eq!(
        ready.status, 200,
        "a degraded site stays in rotation: {text}"
    );
    assert!(
        text.contains("perm.app.unknown")
            && text.contains("bless")
            && text.contains("no_such_permission"),
        "readiness names the code, the method and the permission: {text}"
    );
    let note = make_note(addr, &host, &admin, "to bless").await;
    expect(&note, 201);
    let path = format!(
        "/api/v1/resource/note/{}/methods/bless",
        note.body["data"]["id"].as_str().unwrap()
    );
    // Nobody but the platform itself holds a permission that is declared nowhere.
    expect(
        &http(addr, &host, Some(&admin), "POST", &path, &[], None).await,
        403,
    );
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sample_time_entry_rolls_hours_up_to_its_task_over_http() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let me = me(addr, &host, &admin).await;

    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-RU-1", "title": "Roll up", "status": "Active",
            "owner": me, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let task = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Rolled", "project": project.body["data"]["id"]})),
    )
    .await;
    expect(&task, 201);
    let task_id = task.body["data"]["id"].as_str().unwrap().to_owned();
    let entry = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/time_entry",
        &[],
        Some(json!({
            "task": task_id, "person": me, "created_by": me,
            "work_date": "2026-10-20", "hours": "1.50"
        })),
    )
    .await;
    expect(&entry, 201);
    let entry_path = format!(
        "/api/v1/resource/time_entry/{}",
        entry.body["data"]["id"].as_str().unwrap()
    );
    let logged = |addr, host: String, admin: String, task_id: String| async move {
        let task = http(
            addr,
            &host,
            Some(&admin),
            "GET",
            &format!("/api/v1/resource/task/{task_id}"),
            &[],
            None,
        )
        .await;
        expect(&task, 200);
        task.body["data"]["logged_hours"]
            .as_str()
            .map_or(Decimal::ZERO, |t| t.parse::<Decimal>().unwrap())
    };

    let posted = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        &format!("{entry_path}/post"),
        &[("If-Match", &entry.etag)],
        None,
    )
    .await;
    expect(&posted, 200);
    assert_eq!(
        logged(addr, host.clone(), admin.clone(), task_id.clone()).await,
        Decimal::new(150, 2)
    );
    let cancelled = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        &format!("{entry_path}/cancel"),
        &[("If-Match", &posted.etag)],
        None,
    )
    .await;
    expect(&cancelled, 200);
    assert_eq!(
        logged(addr, host.clone(), admin.clone(), task_id.clone()).await,
        Decimal::ZERO
    );
    started.finish().await;
}
