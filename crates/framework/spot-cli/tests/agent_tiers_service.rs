//! WP-AI-06.T4: agent identity and tiers on a running site (real PostgreSQL through
//! `SPOT_TEST_PG_URL`, skipped when unset). The Administrator's own user is registered as the agent
//! (a row in `spot_agent_tier_registration`, the way the service's store writes it), so the tier is
//! provably the stored one: no header or tool argument changes it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{ADMIN_PASSWORD, Env, Reply, expect, http, token_of};
use serde_json::{Value as Json, json};
use spot_agent_tiers::Tier;
use spot_postgres::PgBackend;
use spot_support_session::{
    ActionScope, Approver, DocTypeScope, Scope, SessionGrant, SessionPolicy, SessionSpec, Subject,
    SubjectKind,
};
use std::net::SocketAddr;

fn rpc(id: u64, method: &str, params: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

async fn tool(
    addr: SocketAddr,
    host: &str,
    token: &str,
    extra: &[(&str, &str)],
    name: &str,
    args: Json,
) -> Reply {
    http(
        addr,
        host,
        Some(token),
        "POST",
        "/mcp",
        extra,
        Some(rpc(
            1,
            "tools/call",
            json!({"name": name, "arguments": args}),
        )),
    )
    .await
}

fn refused(reply: &Reply) -> bool {
    reply.body["error"].is_object() || reply.body["result"]["isError"] == json!(true)
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

async fn backend_of(env: &Env) -> PgBackend {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    PgBackend::connect(loaded.admin.clone().with_dbname(&site.database))
        .await
        .unwrap()
}

async fn register(
    backend: &PgBackend,
    site: &str,
    id: &str,
    domain: &str,
    tier: &str,
    status: &str,
) {
    let client = backend.pool().get().await.unwrap();
    client
        .execute(
            "INSERT INTO spot_agent_tier_registration \
             (site, agent_id, owner, domain, max_tier, deep, status, updated_at) \
             VALUES ($1, $2, 'owner', $3, $4, false, $5, 0) \
             ON CONFLICT (site, agent_id) DO UPDATE SET domain = $3, max_tier = $4, status = $5",
            &[&site, &id, &domain, &tier, &status],
        )
        .await
        .unwrap();
}

async fn put_session(backend: &PgBackend, site: &str, grant: &SessionGrant) {
    let client = backend.pool().get().await.unwrap();
    let text = serde_json::to_string(grant).unwrap();
    client
        .execute(
            "INSERT INTO spot_agent_tier_support_session (site, id, subject_id, grant_json, updated_at) \
             VALUES ($1, $2, $3, $4, 0) \
             ON CONFLICT (site, id) DO UPDATE SET grant_json = $4",
            &[&site, &grant.id, &grant.subject.id, &text],
        )
        .await
        .unwrap();
}

fn grant(tenant: &str, agent: &str, doctypes: &[&str], start: i64, end: i64) -> SessionGrant {
    let spec = SessionSpec {
        id: "support-1".into(),
        tenant: tenant.into(),
        subject: Subject::new(SubjectKind::Agent, agent),
        tier_ceiling: Tier::T0,
        scope: Scope {
            doctypes: DocTypeScope::Named(doctypes.iter().map(|d| (*d).to_owned()).collect()),
            actions: ActionScope::Any,
        },
        reason: "diagnose a ticket".into(),
        requested_by: "support-lead".into(),
        starts_at: start,
        expires_at: end,
    };
    let policy = SessionPolicy::default();
    SessionGrant::request(&spec, &policy, start)
        .unwrap()
        .approve(&Approver::new("customer-owner", true), start, &policy)
        .unwrap()
        .0
}

async fn count(backend: &PgBackend, sql: &str, site: &str) -> i64 {
    let client = backend.pool().get().await.unwrap();
    client.query_one(sql, &[&site]).await.unwrap().get(0)
}

struct World {
    env: Env,
    started: common::Started,
    admin: String,
    admin_id: String,
    task_id: String,
    backend: PgBackend,
}

async fn world() -> Option<World> {
    let env = Env::new("").await?;
    let secret = env.site();
    let started = common::start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = common::admin(addr, &host, &secret).await;
    let admin_id = common::me(addr, &host, &admin).await;
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-AT-1", "title": "Tiers", "status": "Active",
            "owner": admin_id, "currency": "USD", "budget": "10.00"
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
        Some(json!({
            "subject": "Tier test task", "project": project_id
        })),
    )
    .await;
    expect(&task, 201);
    let task_id = task.body["data"]["id"].as_str().unwrap().to_owned();
    let backend = backend_of(&env).await;
    Some(World {
        env,
        started,
        admin,
        admin_id,
        task_id,
        backend,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_t0_agent_reads_and_is_refused_a_create_and_the_tier_cannot_be_claimed() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    register(&w.backend, &host, &w.admin_id, "operations", "T0", "active").await;
    let read = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    expect(&read, 200);
    assert!(!refused(&read), "{:?}", read.body);
    let create = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "create_task_draft",
        json!({"subject": "x"}),
    )
    .await;
    assert!(refused(&create), "{:?}", create.body);

    // A header or an argument that names a tier changes nothing: the tier is the stored one.
    let claimed = tool(
        addr,
        &host,
        &w.admin,
        &[("X-Agent-Tier", "T3"), ("X-Spot-Tier", "T3")],
        "create_task_draft",
        json!({"subject": "x", "tier": "T3", "agent_tier": "T3"}),
    )
    .await;
    assert!(refused(&claimed), "{:?}", claimed.body);

    // A registered T2 agent is still refused a change while the unattended policy is closed (M9).
    register(&w.backend, &host, &w.admin_id, "operations", "T2", "active").await;
    let still = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "create_task_draft",
        json!({"subject": "y"}),
    )
    .await;
    assert!(refused(&still), "{:?}", still.body);
    // Every refusal left a row.
    let rows = count(
        &w.backend,
        "SELECT count(*) FROM spot_agent_tier_event WHERE site = $1 AND event = 'agent.refused'",
        &host,
    )
    .await;
    // Three refusals (the plain create, the claimed tier, the closed policy), three rows.
    assert_eq!(rows, 3);
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suspended_agent_is_refused_everything_at_once() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    register(&w.backend, &host, &w.admin_id, "operations", "T0", "active").await;
    assert!(!refused(
        &tool(
            addr,
            &host,
            &w.admin,
            &[],
            "get_task",
            json!({"id": w.task_id})
        )
        .await
    ));
    register(
        &w.backend,
        &host,
        &w.admin_id,
        "operations",
        "T0",
        "suspended",
    )
    .await;
    let denied = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(refused(&denied), "{:?}", denied.body);
    assert!(denied.body.to_string().contains("agent.suspended"));
    let rows = count(
        &w.backend,
        "SELECT count(*) FROM spot_agent_tier_event WHERE site = $1 AND event = 'agent.refused'",
        &host,
    )
    .await;
    assert_eq!(rows, 1, "the refusal is recorded");
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_support_agent_needs_a_live_scoped_session_and_revocation_is_immediate() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    register(&w.backend, &host, &w.admin_id, "support", "T0", "active").await;
    let none = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(
        refused(&none) && none.body.to_string().contains("session.none"),
        "{:?}",
        none.body
    );

    let t = now();
    let mut g = grant(&host, &w.admin_id, &["Task"], t, t + 3_600);
    put_session(&w.backend, &host, &g).await;
    let ok = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(!refused(&ok), "{:?}", ok.body);
    // Out of scope: another DocType.
    let out = tool(addr, &host, &w.admin, &[], "search_project", json!({})).await;
    assert!(
        refused(&out)
            && out
                .body
                .to_string()
                .contains("session.doctype_not_in_scope")
    );

    // Revocation takes effect on the very next call.
    g.revoke("customer-owner", "done", now()).unwrap();
    put_session(&w.backend, &host, &g).await;
    let revoked = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(refused(&revoked) && revoked.body.to_string().contains("session.revoked"));

    // An expired window refuses (the window closed before now).
    let mut old = grant(&host, &w.admin_id, &["Task"], t, t + 3_600);
    old.starts_at = t - 7_200;
    old.expires_at = t - 3_600;
    put_session(&w.backend, &host, &old).await;
    let expired = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(refused(&expired) && expired.body.to_string().contains("session.expired"));

    // A session of another tenant never opens this one.
    let mut foreign = grant("some-other-site", &w.admin_id, &["Task"], t, t + 3_600);
    foreign.id = "support-1".into();
    put_session(&w.backend, &host, &foreign).await;
    let wrong = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(refused(&wrong) && wrong.body.to_string().contains("session.wrong_tenant"));
    let rows = count(
        &w.backend,
        "SELECT count(*) FROM spot_agent_tier_event WHERE site = $1 AND event = 'agent.refused'",
        &host,
    )
    .await;
    assert!(rows >= 5, "every refusal has a row, got {rows}");
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registrations_and_sessions_survive_a_restart() {
    let Some(w) = world().await else { return };
    let host = w.env.site.clone();
    register(&w.backend, &host, &w.admin_id, "support", "T0", "active").await;
    let t = now();
    put_session(
        &w.backend,
        &host,
        &grant(&host, &w.admin_id, &["Task"], t, t + 3_600),
    )
    .await;
    let World {
        env,
        started,
        task_id,
        ..
    } = w;
    started.finish().await;
    let again = common::start(&env).await;
    let token = token_of(again.addr, &host, "administrator", ADMIN_PASSWORD).await;
    let read = tool(
        again.addr,
        &host,
        &token,
        &[],
        "get_task",
        json!({"id": task_id}),
    )
    .await;
    assert!(!refused(&read), "{:?}", read.body);
    again.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_user_is_not_tier_checked() {
    let Some(w) = world().await else { return };
    let (addr, host) = (w.started.addr, w.env.site.clone());
    // No registration: the Administrator is a person, with the rights of a person.
    let create = tool(
        addr,
        &host,
        &w.admin,
        &[],
        "get_task",
        json!({"id": w.task_id}),
    )
    .await;
    assert!(!refused(&create), "{:?}", create.body);
    w.started.finish().await;
}
