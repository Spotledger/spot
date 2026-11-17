//! WP-AI-05.T4: AI workload isolation on a running site (real PostgreSQL through
//! `SPOT_TEST_PG_URL`, skipped when unset). The 20-agent human-latency run over the whole server is
//! the `l3-http` case `ai_isolation`; this file proves the hosting: the `ai` role exists with no write
//! right, MCP reads run on its own pool, a role that cannot read makes MCP refuse instead of using the
//! main pool, and a site that cannot set isolation up refuses MCP (503) rather than serving it unisolated.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Env, Reply, expect, http};
use serde_json::{Value as Json, json};
use spot_postgres::{PgBackend, PgConfig};
use std::net::SocketAddr;
use std::path::PathBuf;

const ROLE_PASSWORD: &str = "ai-role.password_for~tests-42";

fn rpc(id: u64, method: &str, params: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

async fn mcp(addr: SocketAddr, host: &str, token: &str, method: &str, params: Json) -> Reply {
    http(
        addr,
        host,
        Some(token),
        "POST",
        "/mcp",
        &[],
        Some(rpc(1, method, params)),
    )
    .await
}

fn refused(reply: &Reply) -> bool {
    reply.body["error"].is_object() || reply.body["result"]["isError"] == json!(true)
}

fn password_file() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "spot_ai_pw_{}",
        spot_storage_contract::RecordId::new_v7()
    ));
    std::fs::write(&path, format!("{ROLE_PASSWORD}\n")).unwrap();
    path
}

async fn site_pg(env: &Env) -> PgConfig {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    loaded.admin.clone().with_dbname(&site.database)
}

struct World {
    env: Env,
    started: common::Started,
    admin: String,
    pw: Option<PathBuf>,
}

async fn world(with_password: bool) -> Option<World> {
    let pw = with_password.then(password_file);
    let services = match &pw {
        Some(path) => format!(
            "\n[services.ai_isolation]\nenabled = true\npool_size = 2\npassword_file = {:?}\n",
            path.display().to_string()
        ),
        None => "\n[services.ai_isolation]\nenabled = true\n".to_owned(),
    };
    let env = Env::new(&services).await?;
    let secret = env.site();
    let started = common::start(&env).await;
    let admin = common::admin(started.addr, &env.site, &secret).await;
    let me = common::me(started.addr, &env.site, &admin).await;
    let project = http(
        started.addr,
        &env.site,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-AI-1", "title": "Isolation", "status": "Active",
            "owner": me, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project_id = project.body["data"]["id"].as_str().unwrap().to_owned();
    let task = http(
        started.addr,
        &env.site,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({
            "subject": "Read on the ai pool", "project": project_id
        })),
    )
    .await;
    expect(&task, 201);
    Some(World {
        env,
        started,
        admin,
        pw,
    })
}

impl World {
    async fn done(self) {
        self.started.finish().await;
        if let Some(path) = self.pw {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_run_on_the_ai_pool_and_the_role_cannot_write() {
    let Some(w) = world(true).await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let found = mcp(
        addr,
        &host,
        &w.admin,
        "tools/call",
        json!({"name": "search_task", "arguments": {}}),
    )
    .await;
    expect(&found, 200);
    assert!(!refused(&found), "{:?}", found.body);
    assert!(
        found.body.to_string().contains("Read on the ai pool"),
        "{:?}",
        found.body
    );

    // The pool is the ai role's: a connection as the role reads, and an INSERT is refused by the
    // server (no grant, and the role is read-only by default).
    let mut config = site_pg(&w.env).await;
    config.user = "spot_ai".to_owned();
    config.password = ROLE_PASSWORD.to_owned();
    let as_role = PgBackend::connect(config).await.unwrap();
    let client = as_role.pool().get().await.unwrap();
    let user: String = client
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(user, "spot_ai");
    assert!(
        client
            .execute("INSERT INTO role DEFAULT VALUES", &[])
            .await
            .is_err()
    );
    assert!(
        client
            .execute("CREATE TABLE ai_must_not (x int)", &[])
            .await
            .is_err()
    );
    drop(client);
    w.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_that_cannot_read_makes_mcp_refuse_instead_of_using_the_main_pool() {
    let Some(w) = world(true).await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let ok = mcp(
        addr,
        &host,
        &w.admin,
        "tools/call",
        json!({"name": "search_task", "arguments": {}}),
    )
    .await;
    assert!(!refused(&ok), "{:?}", ok.body);
    let admin_backend = PgBackend::connect(site_pg(&w.env).await).await.unwrap();
    admin_backend
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute("REVOKE SELECT ON ALL TABLES IN SCHEMA public FROM spot_ai")
        .await
        .unwrap();
    let denied = mcp(
        addr,
        &host,
        &w.admin,
        "tools/call",
        json!({"name": "search_task", "arguments": {}}),
    )
    .await;
    assert!(
        refused(&denied),
        "the read must fail, not fall back: {:?}",
        denied.body
    );
    assert!(!denied.body.to_string().contains("Read on the ai pool"));
    // REST, on the main pool, is untouched.
    let rest = http(
        addr,
        &host,
        Some(&w.admin),
        "GET",
        "/api/v1/resource/task",
        &[],
        None,
    )
    .await;
    expect(&rest, 200);
    w.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_password_file_mcp_is_refused_not_served_unisolated() {
    let Some(w) = world(false).await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    let reply = mcp(addr, &host, &w.admin, "tools/list", json!({})).await;
    expect(&reply, 503);
    assert_eq!(reply.body["error"]["code"], json!("server.unavailable"));
    // The rest of the site serves.
    let rest = http(
        addr,
        &host,
        Some(&w.admin),
        "GET",
        "/api/v1/resource/task",
        &[],
        None,
    )
    .await;
    expect(&rest, 200);
    w.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_password_file_refuses_mcp_too() {
    let path = std::env::temp_dir().join(format!(
        "spot_ai_bad_{}",
        spot_storage_contract::RecordId::new_v7()
    ));
    std::fs::write(&path, "short\n").unwrap();
    let services = format!(
        "\n[services.ai_isolation]\nenabled = true\npassword_file = {:?}\n",
        path.display().to_string()
    );
    let Some(env) = Env::new(&services).await else {
        return;
    };
    let secret = env.site();
    let started = common::start(&env).await;
    let admin = common::admin(started.addr, &env.site, &secret).await;
    let reply = mcp(started.addr, &env.site, &admin, "tools/list", json!({})).await;
    expect(&reply, 503);
    started.finish().await;
    let _ = std::fs::remove_file(path);
}
