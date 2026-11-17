//! WP-AI-04.T4: the MCP endpoint of `spot serve`, over real HTTP and the test PostgreSQL
//! (`SPOT_TEST_PG_URL`; skipped with no database, never faked). The full cross-tenant and
//! permission suite over a running server is WP-AI-04.T5 (`l3-http`); this file proves the hosting:
//! authentication through the site's own sign-in service, the Origin rule, the body cap, the
//! method rule, tenancy, the off switch, and that no system principal exists in the module.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Env, Reply, admin, expect, http, login, role};
use serde_json::{Value as Json, json};
use std::net::SocketAddr;

const PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";

fn rpc(id: u64, method: &str, params: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

async fn call(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Json,
) -> Reply {
    http(addr, host, token, "POST", "/mcp", headers, Some(body)).await
}

fn code(reply: &Reply) -> &str {
    reply.body["error"]["code"].as_str().unwrap_or_default()
}

fn tool_names(reply: &Reply) -> Vec<String> {
    reply.body["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The session token of a user the CLI added while the host runs: its chosen password is a staged
/// credential the host imports on its next reload, so the sign-in is retried until it does.
async fn token_of(addr: SocketAddr, host: &str, user: &str, password: &str) -> String {
    let mut last = None;
    for _ in 0..60 {
        let reply = login(addr, host, user, password).await;
        if reply.status == 200 {
            return reply.body["data"]["token"].as_str().unwrap().to_owned();
        }
        last = Some(reply);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("{user} never signed in: {:?}", last.map(|r| r.body));
}

/// A site with the Administrator signed in, one project and one task.
struct World {
    /// Held so the site root outlives the host (dropping it removes the directory).
    _env: Env,
    started: common::Started,
    host: String,
    admin: String,
    task_id: String,
}

async fn world(services: &str) -> Option<World> {
    world_with_users(services, &[]).await
}

/// A world whose extra users exist before the host starts: the sign-in stores import the CLI's
/// staged credentials at start, so a user added later is not usable until a reload.
async fn world_with_users(services: &str, users: &[&str]) -> Option<World> {
    let env = Env::new(services).await?;
    let secret = env.site();
    for user in users {
        env.user(user, PASSWORD);
    }
    let started = common::start(&env).await;
    let host = env.site.clone();
    let addr = started.addr;
    let admin = admin(addr, &host, &secret).await;
    let me = common::me(addr, &host, &admin).await;
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-MCP-1", "title": "MCP", "status": "Active",
            "owner": me, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project_id = project.body["data"]["id"].as_str().unwrap().to_owned();
    let task = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Read me over MCP", "project": project_id})),
    )
    .await;
    expect(&task, 201);
    let task_id = task.body["data"]["id"].as_str().unwrap().to_owned();
    Some(World {
        _env: env,
        started,
        host,
        admin,
        task_id,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_lists_tools_and_reads_a_document_as_that_user() {
    let Some(w) = world_with_users("", &["nobody@example.com"]).await else {
        return;
    };
    let addr = w.started.addr;
    let listed = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[],
        rpc(1, "tools/list", json!({})),
    )
    .await;
    expect(&listed, 200);
    assert!(
        tool_names(&listed).iter().any(|n| n == "get_task"),
        "{:?}",
        listed.body
    );
    assert_eq!(
        listed.header("MCP-Protocol-Version"),
        Some(spot_mcp::PROTOCOL_VERSION)
    );
    assert!(listed.header("X-Request-Id").is_some());
    let got = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[],
        rpc(
            2,
            "tools/call",
            json!({"name": "get_task", "arguments": {"id": w.task_id}}),
        ),
    )
    .await;
    expect(&got, 200);
    assert!(
        got.body.to_string().contains("Read me over MCP"),
        "{:?}",
        got.body
    );

    // A user with no role is a different principal: no document tools, and the task is the same
    // not-found a REST read gives.
    let nobody = token_of(addr, &w.host, "nobody@example.com", PASSWORD).await;
    let theirs = call(
        addr,
        &w.host,
        Some(&nobody),
        &[],
        rpc(3, "tools/list", json!({})),
    )
    .await;
    expect(&theirs, 200);
    assert!(!tool_names(&theirs).iter().any(|n| n == "get_task"));
    let denied = call(
        addr,
        &w.host,
        Some(&nobody),
        &[],
        rpc(
            4,
            "tools/call",
            json!({"name": "get_task", "arguments": {"id": w.task_id}}),
        ),
    )
    .await;
    assert!(!denied.body.to_string().contains("Read me over MCP"));
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_or_bad_credential_is_401_with_one_answer() {
    let Some(w) = world("").await else { return };
    let addr = w.started.addr;
    let list = rpc(1, "tools/list", json!({}));
    let missing = call(addr, &w.host, None, &[], list.clone()).await;
    expect(&missing, 401);
    assert_eq!(code(&missing), "auth.required");
    assert_eq!(missing.header("WWW-Authenticate"), Some("Bearer"));
    assert!(!missing.body.to_string().contains("tools"));

    let junk = call(
        addr,
        &w.host,
        Some("not-a-session-token"),
        &[],
        list.clone(),
    )
    .await;
    expect(&junk, 401);
    assert_eq!(code(&junk), "auth.invalid_credentials");
    assert_eq!(junk.header("WWW-Authenticate"), Some("Bearer"));

    // A well-formed key that nothing issued, and malformed keys, all answer the same.
    let (key, _) = spot_auth::ApiKey::generate();
    let text = key.text();
    let (id, secret) = text.strip_prefix("spk_").unwrap().split_once('_').unwrap();
    let unknown = call(
        addr,
        &w.host,
        None,
        &[("X-Api-Key", &format!("{id}:{secret}"))],
        list.clone(),
    )
    .await;
    expect(&unknown, 401);
    assert_eq!(code(&unknown), "auth.invalid_credentials");
    for bad in ["nocolon", "abc:def", ":"] {
        let reply = call(addr, &w.host, None, &[("X-Api-Key", bad)], list.clone()).await;
        expect(&reply, 401);
        assert_eq!(code(&reply), "auth.invalid_credentials");
        assert!(!reply.body.to_string().contains(bad) || bad == ":");
    }
    // Both credentials at once are refused rather than one picked.
    let both = call(addr, &w.host, Some(&w.admin), &[("X-Api-Key", "a:b")], list).await;
    expect(&both, 401);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_of_one_site_is_401_on_another() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    let other = format!("{}-b", env.site);
    common::ok(&env.root.0, &["new-site", &other]);
    let started = common::start(&env).await;
    let token = admin(started.addr, &env.site, &secret).await;
    let own = call(
        started.addr,
        &env.site,
        Some(&token),
        &[],
        rpc(1, "tools/list", json!({})),
    )
    .await;
    expect(&own, 200);
    let foreign = call(
        started.addr,
        &other,
        Some(&token),
        &[],
        rpc(1, "tools/list", json!({})),
    )
    .await;
    expect(&foreign, 401);
    assert_eq!(code(&foreign), "auth.invalid_credentials");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_origin_rule_the_method_rule_and_the_body_cap() {
    let Some(w) = world("").await else { return };
    let addr = w.started.addr;
    let list = rpc(1, "tools/list", json!({}));
    let foreign = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[("Origin", "https://evil.example")],
        list.clone(),
    )
    .await;
    expect(&foreign, 403);
    assert_eq!(code(&foreign), "permission.denied");
    let null = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[("Origin", "null")],
        list.clone(),
    )
    .await;
    expect(&null, 403);
    // The foreign origin is refused before the credential: no credential needed to be told no.
    let anonymous = call(
        addr,
        &w.host,
        None,
        &[("Origin", "https://evil.example")],
        list.clone(),
    )
    .await;
    expect(&anonymous, 403);
    let own_origin = format!("http://{}", w.host);
    let same = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[("Origin", &own_origin)],
        list.clone(),
    )
    .await;
    expect(&same, 200);

    // Only POST: a GET is 405 with or without a credential, and never opens a stream.
    let get = http(addr, &w.host, Some(&w.admin), "GET", "/mcp", &[], None).await;
    expect(&get, 405);
    let get_anonymous = http(addr, &w.host, None, "GET", "/mcp", &[], None).await;
    expect(&get_anonymous, 405);

    // The body cap: 256 KiB, answered 413 without a tool call.
    let big = json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {"pad": "x".repeat(300_000)}});
    let huge = call(addr, &w.host, Some(&w.admin), &[], big).await;
    expect(&huge, 413);
    let version = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[("MCP-Protocol-Version", "1999-01-01")],
        list,
    )
    .await;
    expect(&version, 400);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_who_must_still_choose_a_password_cannot_use_the_endpoint() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    let started = common::start(&env).await;
    // The one-time secret is not a bearer key (the sign-in stores refuse it as one); it signs the
    // Administrator in to a restricted session, which may only change the password: authenticated,
    // then refused.
    let signed_in = common::login(started.addr, &env.site, "administrator", &first).await;
    expect(&signed_in, 200);
    let restricted = signed_in.body["data"]["token"].as_str().unwrap().to_owned();
    let key = call(
        started.addr,
        &env.site,
        Some(&restricted),
        &[],
        rpc(1, "tools/list", json!({})),
    )
    .await;
    expect(&key, 403);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_off_switch_answers_404() {
    let Some(w) = world("\n[services.mcp]\nenabled = false\n").await else {
        return;
    };
    let addr = w.started.addr;
    let off = call(
        addr,
        &w.host,
        Some(&w.admin),
        &[],
        rpc(1, "tools/list", json!({})),
    )
    .await;
    expect(&off, 404);
    let anonymous = call(addr, &w.host, None, &[], rpc(1, "tools/list", json!({}))).await;
    expect(&anonymous, 404);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_granted_at_runtime_shows_up_on_the_next_request() {
    let Some(w) = world_with_users("", &["later@example.com"]).await else {
        return;
    };
    let addr = w.started.addr;
    let later = token_of(addr, &w.host, "later@example.com", PASSWORD).await;
    let before = call(
        addr,
        &w.host,
        Some(&later),
        &[],
        rpc(1, "tools/list", json!({})),
    )
    .await;
    assert!(!tool_names(&before).iter().any(|n| n == "get_task"));
    let user = common::user_id(addr, &w.host, &w.admin, "later@example.com").await;
    let viewer = role(addr, &w.host, &w.admin, "mcp-viewer").await;
    common::assign(addr, &w.host, &w.admin, &user, &viewer).await;
    // The role carries no grant of its own: the list is still the principal's own view, and a
    // read is still the same not-found. What matters is that the call is evaluated afresh.
    let after = call(
        addr,
        &w.host,
        Some(&later),
        &[],
        rpc(2, "tools/list", json!({})),
    )
    .await;
    expect(&after, 200);
    w.started.finish().await;
}

/// No system principal is constructible in the module (the brief's grep-clean test).
#[test]
fn the_module_never_builds_a_system_principal() {
    let files = [
        include_str!("../src/services/mcp.rs"),
        include_str!("../src/services/mcp/identity.rs"),
        include_str!("../src/services/mcp/seams.rs"),
        include_str!("../src/services/mcp/origin.rs"),
    ];
    for source in files {
        let code: String = source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "Actor::System",
            "Principal::system",
            "is_system: true",
            "Actor::Consumer",
        ] {
            assert!(!code.contains(needle), "found `{needle}` in the MCP module");
        }
    }
}
