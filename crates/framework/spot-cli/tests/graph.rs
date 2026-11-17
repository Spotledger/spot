//! `graph_query` over a real `spot serve` on real PostgreSQL (skipped, with no output, when
//! `SPOT_TEST_PG_URL` is unset): the edges a document write leaves, the hard limits, the
//! permission filter and the one answer for an unknown and an unreadable start.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use spot_storage_contract::RecordId;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

struct Reply {
    status: u16,
    raw: String,
    etag: String,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_str(&self.raw).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
}

async fn http(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Json>,
) -> Reply {
    http_with(addr, host, token, method, path, body, &[]).await
}

async fn http_with(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Json>,
    extra: &[(&str, &str)],
) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").unwrap();
    Reply {
        status: headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        etag: headers
            .lines()
            .find_map(|l| {
                l.strip_prefix("etag: ")
                    .or_else(|| l.strip_prefix("ETag: "))
            })
            .unwrap_or_default()
            .to_owned(),
        raw: payload.to_owned(),
    }
}

/// What a test changes in the stack before it starts.
#[derive(Default)]
struct Cfg<'a> {
    /// Lines under `[services.graph]` in `common.toml`.
    services: &'a str,
    /// Text appended to the sample app's `permissions/grants.toml`.
    grants: &'a str,
    /// Sites besides the first, in the same server.
    extra_sites: usize,
}

/// One site of the server and the Administrator's session on it.
struct Tenant {
    host: String,
    admin: String,
    admin_id: String,
}

/// Creates the site's Administrator session: the one-time secret is changed for `PASSWORD`.
async fn sign_in(addr: SocketAddr, host: &str, secret: &str) -> Tenant {
    let first = http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": "administrator", "password": secret})),
    )
    .await;
    let token = first.json()["data"]["token"].as_str().unwrap().to_owned();
    let changed = http(
        addr,
        host,
        Some(&token),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": PASSWORD})),
    )
    .await;
    let admin = changed.json()["data"]["token"].as_str().unwrap().to_owned();
    let who = http(
        addr,
        host,
        Some(&admin),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    let admin_id = who.json()["data"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Tenant {
        host: host.to_owned(),
        admin,
        admin_id,
    }
}

struct Stack {
    env: Env,
    addr: SocketAddr,
    host: String,
    admin: String,
    admin_id: String,
    others: Vec<Tenant>,
    server: Option<Running>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Stack {
    async fn start_with(cfg: Cfg<'_>) -> Option<Stack> {
        let env = Env::new().await?;
        let common = env.root.join("common.toml");
        let mut text = std::fs::read_to_string(&common).unwrap();
        text.push_str(&format!("[services.graph]\n{}\n", cfg.services));
        std::fs::write(&common, text).unwrap();
        if !cfg.grants.is_empty() {
            let grants = env.root.join("sample/permissions/grants.toml");
            let mut text = std::fs::read_to_string(&grants).unwrap();
            text.push_str(&format!("\n{}\n", cfg.grants));
            std::fs::write(&grants, text).unwrap();
        }
        let secret = env
            .ok(&["new-site", &env.site])
            .trim()
            .rsplit(' ')
            .next()
            .unwrap()
            .to_owned();
        let mut more = Vec::new();
        for _ in 0..cfg.extra_sites {
            let name = format!(
                "spot-test-{}",
                RecordId::new_v7().to_string().replace('-', "")
            );
            let key = env
                .ok(&["new-site", &name])
                .trim()
                .rsplit(' ')
                .next()
                .unwrap()
                .to_owned();
            more.push((name, key));
        }
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = spot_cli::start_with(
            &env.root,
            false,
            Options {
                clock: None,
                login: LoginConfig::for_tests(),
            },
            async {
                let _ = stopped.await;
            },
        )
        .await
        .unwrap();
        let addr = server.local_addrs()[0];
        let host = env.site.clone();
        let first = sign_in(addr, &host, &secret).await;
        let mut others = Vec::new();
        for (name, key) in &more {
            others.push(sign_in(addr, name, key).await);
        }
        Some(Stack {
            env,
            addr,
            host,
            admin: first.admin,
            admin_id: first.admin_id,
            others,
            server: Some(server),
            stop: Some(stop),
        })
    }

    async fn call(&self, method: &str, path: &str, body: Option<Json>) -> Reply {
        http(self.addr, &self.host, Some(&self.admin), method, path, body).await
    }

    async fn create_on(&self, tenant: &Tenant, doctype: &str, body: Json) -> String {
        let made = http(
            self.addr,
            &tenant.host,
            Some(&tenant.admin),
            "POST",
            &format!("/api/v1/resource/{doctype}"),
            Some(body),
        )
        .await;
        assert_eq!(made.status, 201, "{}", made.raw);
        made.json()["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn create(&self, doctype: &str, body: Json) -> String {
        let me = Tenant {
            host: self.host.clone(),
            admin: self.admin.clone(),
            admin_id: self.admin_id.clone(),
        };
        self.create_on(&me, doctype, body).await
    }

    /// A project of the first site, owned by its Administrator.
    async fn project(&self, code: &str) -> String {
        let owner = self.admin_id.clone();
        self.create(
            "project",
            json!({"code": code, "title": code, "status": "Active", "owner": owner}),
        )
        .await
    }

    async fn task(&self, subject: &str, project: &str) -> String {
        self.create("task", json!({"subject": subject, "project": project}))
            .await
    }

    /// A user of the first site holding only the role `role`; returns the session token.
    async fn user_with_role(&self, email: &str, role: &str) -> String {
        self.env.ok(&["add-user", &self.env.site, email]);
        let set = support::run(
            &self.env.root,
            &["set-password", &self.env.site, email, "--password-stdin"],
            Some(&format!("{PASSWORD}\n")),
        );
        assert!(set.status.success(), "{}", support::text(&set));
        let users = self
            .call("GET", "/api/v1/resource/user?limit=100", None)
            .await;
        let user = users.json()["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["email"] == email)
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .unwrap();
        let made = self
            .call(
                "POST",
                "/api/v1/resource/role",
                Some(json!({"role_name": role, "origin": "reference"})),
            )
            .await;
        assert_eq!(made.status, 201, "{}", made.raw);
        let role_id = made.json()["data"]["id"].as_str().unwrap().to_owned();
        let assigned = self
            .call(
                "POST",
                "/api/v1/resource/role_assignment",
                Some(json!({
                    "user": user, "role": role_id, "source": "manual", "state": "active",
                    "reason": "Assigned by the test operator."
                })),
            )
            .await;
        assert_eq!(assigned.status, 201, "{}", assigned.raw);
        let login = http(
            self.addr,
            &self.host,
            None,
            "POST",
            "/api/v1/auth/login",
            Some(json!({"user": email, "password": PASSWORD})),
        )
        .await;
        assert_eq!(login.status, 200, "{}", login.raw);
        login.json()["data"]["token"].as_str().unwrap().to_owned()
    }

    async fn graph(&self, token: &str, body: Json) -> Reply {
        http(
            self.addr,
            &self.host,
            Some(token),
            "POST",
            "/api/v1/graph/query",
            Some(body),
        )
        .await
    }

    /// The ids of the nodes of an answer to `body` as the Administrator (empty unless 200).
    async fn node_ids(&self, body: Json) -> BTreeSet<String> {
        let reply = self.graph(&self.admin, body).await;
        if reply.status != 200 {
            return BTreeSet::new();
        }
        reply.json()["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Polls (up to 30 s) until the Administrator's answer to `body` satisfies `done`.
    async fn until(
        &self,
        body: Json,
        done: impl Fn(&BTreeSet<String>) -> bool,
    ) -> BTreeSet<String> {
        let mut last = BTreeSet::new();
        for _ in 0..60 {
            last = self.node_ids(body.clone()).await;
            if done(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("the graph answer never changed as expected: {last:?}");
    }

    /// The site's database, to change the edge table behind the feed's back.
    async fn pool(&self) -> deadpool_postgres::Object {
        let loaded = spot_cli::config::load(&self.env.root, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap()
            .clone();
        let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
        backend.pool().get().await.unwrap()
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.join().await;
        }
    }
}

fn query(project: &str, depth: u64) -> Json {
    query_from("project", project, "in", depth)
}

fn query_from(doctype: &str, id: &str, direction: &str, depth: u64) -> Json {
    json!({"start": {"doctype": doctype, "id": id}, "direction": direction,
           "edge_types": ["task_project"], "depth": depth, "limit": 100})
}

/// A faster feed and drift check than the default, so a test need not wait minutes.
const FAST: &str = "feed_interval_ms = 50\ndrift_interval_secs = 1\nindex_memory_mb = 64";
/// The index off: every query runs over the database.
const STORAGE_ONLY: &str = "index_memory_mb = 0";
/// A role that reads Projects and nothing else.
const PROJECT_READER: &str = "[[entries]]\nrole = \"Project Reader\"\ndoctype = \"project\"\nrights = [\"read\", \"select\"]";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_is_reached_from_its_project_and_the_hard_limits_hold() {
    // The index follows the change feed; the faster feed keeps the wait for the edge short.
    let Some(stack) = Stack::start_with(Cfg {
        services: FAST,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("ALPHA").await;
    let task = stack.task("Write", &project).await;
    // The edge reaches the index through the change feed, within its interval.
    stack
        .until(query(&project, 2), |ids| ids.contains(&task))
        .await;
    let found = stack
        .call("POST", "/api/v1/graph/query", Some(query(&project, 2)))
        .await;
    assert_eq!(found.status, 200, "{}", found.raw);
    let data = found.json()["data"].clone();
    assert!(
        data["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == task.as_str()),
        "{data}"
    );
    assert_eq!(data["withheld"], false);
    let deep = stack
        .call("POST", "/api/v1/graph/query", Some(query(&project, 9)))
        .await;
    assert_eq!(
        (deep.status, deep.code().as_str()),
        (422, "graph.too_large"),
        "{}",
        deep.raw
    );
    let gone = stack
        .call("POST", "/api/v1/graph/query", Some(query(&support_id(), 2)))
        .await;
    assert_eq!(
        (gone.status, gone.code().as_str()),
        (404, "graph.not_found"),
        "{}",
        gone.raw
    );
    let anonymous = http(
        stack.addr,
        &stack.host,
        None,
        "POST",
        "/api/v1/graph/query",
        Some(query(&project, 2)),
    )
    .await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.raw);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_created_link_appears_and_a_deleted_document_takes_its_edge_away() {
    let Some(stack) = Stack::start_with(Cfg {
        services: FAST,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("DEL").await;
    let kept = stack.task("Kept", &project).await;
    let doomed = stack.task("Doomed", &project).await;
    let both = stack
        .until(query(&project, 1), |ids| {
            ids.contains(&kept) && ids.contains(&doomed)
        })
        .await;
    assert!(both.contains(&kept) && both.contains(&doomed));
    let path = format!("/api/v1/resource/task/{doomed}");
    let current = stack.call("GET", &path, None).await;
    assert_eq!(current.status, 200, "{}", current.raw);
    let deleted = http_with(
        stack.addr,
        &stack.host,
        Some(&stack.admin),
        "DELETE",
        &path,
        None,
        &[("If-Match", &current.etag)],
    )
    .await;
    assert!(deleted.status < 300, "{}", deleted.raw);
    let after = stack
        .until(query(&project, 1), |ids| !ids.contains(&doomed))
        .await;
    assert!(after.contains(&kept), "{after:?}");
    stack.finish().await;
}

/// The hidden-node case, once with the index and once over the database only.
async fn hidden_node_case(services: &str) {
    let Some(stack) = Stack::start_with(Cfg {
        services,
        grants: PROJECT_READER,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("HID").await;
    let task = stack.task("Secret", &project).await;
    let reader = stack
        .user_with_role("reader@example.com", "Project Reader")
        .await;
    let seen = stack
        .until(query(&project, 1), |ids| ids.contains(&task))
        .await;
    assert!(seen.contains(&task));
    let admin_view = stack.graph(&stack.admin, query(&project, 1)).await;
    assert_eq!(admin_view.json()["data"]["withheld"], false);
    // Give the index time to become hot, then ask as the reader on both paths' timing.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let hidden = stack.graph(&reader, query(&project, 2)).await;
    assert_eq!(hidden.status, 200, "{}", hidden.raw);
    let data = hidden.json()["data"].clone();
    assert!(
        data["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["id"] != task.as_str()),
        "{data}"
    );
    assert!(
        data["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["from"]["id"] != task.as_str() && e["to"]["id"] != task.as_str()),
        "{data}"
    );
    assert_eq!(data["withheld"], true, "{data}");
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hidden_node_removes_its_paths_and_withheld_is_true_over_the_index() {
    hidden_node_case(FAST).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hidden_node_removes_its_paths_and_withheld_is_true_over_the_database() {
    hidden_node_case(STORAGE_ONLY).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_without_a_right_to_the_start_gets_the_answer_of_an_unknown_record() {
    let Some(stack) = Stack::start_with(Cfg {
        services: FAST,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("NOROLE").await;
    stack.task("T", &project).await;
    // A user with a role that grants nothing on Projects: the permission source says no, and the
    // answer is the one an unknown record gets (it fails closed, never open).
    let nobody = stack.user_with_role("nobody@example.com", "Nothing").await;
    let denied = stack.graph(&nobody, query(&project, 1)).await;
    let unknown = stack.graph(&nobody, query(&support_id(), 1)).await;
    assert_eq!(
        (denied.status, denied.code().as_str()),
        (404, "graph.not_found"),
        "{}",
        denied.raw
    );
    assert_eq!(
        (unknown.status, unknown.code()),
        (denied.status, denied.code())
    );
    let forged = stack.graph("not-a-session-token", query(&project, 1)).await;
    assert_eq!(forged.status, 401, "{}", forged.raw);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_removed_behind_the_feed_is_found_by_the_drift_check_and_the_index_rebuilds() {
    let Some(stack) = Stack::start_with(Cfg {
        services: FAST,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("GAP").await;
    let first = stack.task("One", &project).await;
    let second = stack.task("Two", &project).await;
    stack
        .until(query(&project, 1), |ids| {
            ids.contains(&first) && ids.contains(&second)
        })
        .await;
    // The first answers came from the database; let the index build and take over.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let client = stack.pool().await;
    let removed = client
        .execute(
            "DELETE FROM spot_graph_edge WHERE from_id::text = $1 OR to_id::text = $1",
            &[&first],
        )
        .await
        .unwrap();
    assert!(removed >= 1, "the edge table held no edge of the task");
    // No event announced the change: only the drift check can see it. The documents are
    // authoritative, so it repairs the edge table from them (the removed edge comes back) and the
    // hot index follows the table: the answer holds the task again.
    let after = stack
        .until(query(&project, 1), |ids| {
            ids.contains(&first) && ids.contains(&second)
        })
        .await;
    assert!(after.contains(&second), "{after:?}");
    // The repair writes the edge table back within a few drift intervals.
    let mut restored: i64 = 0;
    for _ in 0..60 {
        restored = client
            .query_one(
                &format!(
                    "{}{}",
                    "SELECT count(*) FR",
                    "OM spot_graph_edge WHERE from_id::text = $1 OR to_id::text = $1"
                ),
                &[&first],
            )
            .await
            .unwrap()
            .get(0);
        if restored >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        restored >= 1,
        "the drift check repaired the edge table ({restored} edges, {removed} removed)"
    );
    stack.finish().await;
}

/// One answer without ids: node and edge names by the order the records were created in.
fn shape(data: &Json, names: &HashMap<String, String>) -> (BTreeSet<String>, BTreeSet<String>) {
    let name = |id: &Json| names.get(id.as_str().unwrap()).cloned().unwrap();
    let nodes = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| name(&n["id"]))
        .collect();
    let edges = data["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{}:{}>{}",
                e["type"].as_str().unwrap(),
                name(&e["from"]["id"]),
                name(&e["to"]["id"])
            )
        })
        .collect();
    (nodes, edges)
}

/// A seeded graph (4 projects, 12 tasks, the assignment fixed by a formula), every query shape
/// over it, answered without ids: the same on every stack that holds the same graph.
async fn seeded_answers(stack: &Stack) -> Vec<String> {
    let mut names = HashMap::new();
    let mut projects = Vec::new();
    for p in 0..4 {
        let id = stack.project(&format!("SEED{p}")).await;
        names.insert(id.clone(), format!("p{p}"));
        projects.push(id);
    }
    let mut tasks = Vec::new();
    for t in 0..12usize {
        let home = (t * 5 + t / 3) % 4;
        let id = stack.task(&format!("seed {t}"), &projects[home]).await;
        names.insert(id.clone(), format!("t{t}"));
        tasks.push(id);
    }
    // The first query starts the index build; later ones run on the index once it is hot.
    stack.node_ids(query(&projects[0], 1)).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mut out = Vec::new();
    {
        for (p, id) in projects.iter().enumerate() {
            for direction in ["in", "out", "both"] {
                for depth in 1..=3 {
                    let reply = stack
                        .graph(&stack.admin, query_from("project", id, direction, depth))
                        .await;
                    assert_eq!(reply.status, 200, "{}", reply.raw);
                    out.push(format!(
                        "p{p} {direction} {depth} {:?}",
                        shape(&reply.json()["data"], &names)
                    ));
                }
            }
        }
        for (t, id) in tasks.iter().enumerate() {
            for direction in ["out", "both"] {
                for depth in 1..=2 {
                    let reply = stack
                        .graph(&stack.admin, query_from("task", id, direction, depth))
                        .await;
                    assert_eq!(reply.status, 200, "{}", reply.raw);
                    out.push(format!(
                        "t{t} {direction} {depth} {:?}",
                        shape(&reply.json()["data"], &names)
                    ));
                }
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_index_and_the_database_give_identical_answers_over_a_seeded_graph() {
    let Some(storage) = Stack::start_with(Cfg {
        services: STORAGE_ONLY,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let from_storage = seeded_answers(&storage).await;
    storage.finish().await;
    let Some(indexed) = Stack::start_with(Cfg {
        services: FAST,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let from_index = seeded_answers(&indexed).await;
    indexed.finish().await;
    assert!(from_storage.iter().any(|line| line.contains("t0")));
    assert_eq!(from_storage, from_index);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_index_off_every_query_still_answers_correctly() {
    let Some(stack) = Stack::start_with(Cfg {
        services: STORAGE_ONLY,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("OFF").await;
    let task = stack.task("Direct", &project).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let ids = stack.node_ids(query(&project, 1)).await;
    assert!(ids.contains(&task), "{ids:?}");
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_of_one_server_share_no_graph() {
    let Some(stack) = Stack::start_with(Cfg {
        services: FAST,
        extra_sites: 1,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let other = &stack.others[0];
    let a_project = stack.project("SITEA").await;
    let a_task = stack.task("A task", &a_project).await;
    let b_project = stack
        .create_on(
            other,
            "project",
            json!({"code": "SITEB", "title": "B", "status": "Active", "owner": other.admin_id}),
        )
        .await;
    let b_task = stack
        .create_on(
            other,
            "task",
            json!({"subject": "B task", "project": b_project}),
        )
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let ask = |tenant: &Tenant, token: &str, project: &str| {
        // Owned copies: the future outlives the borrows the closure takes.
        let (addr, host, token, project) = (
            stack.addr,
            tenant.host.clone(),
            token.to_owned(),
            project.to_owned(),
        );
        async move {
            http(
                addr,
                &host,
                Some(&token),
                "POST",
                "/api/v1/graph/query",
                Some(query(&project, 2)),
            )
            .await
        }
    };
    let mine = Tenant {
        host: stack.host.clone(),
        admin: stack.admin.clone(),
        admin_id: stack.admin_id.clone(),
    };
    // Each site sees its own graph and nothing of the other.
    let on_b = ask(other, &other.admin, &b_project).await;
    assert_eq!(on_b.status, 200, "{}", on_b.raw);
    let b_ids: BTreeSet<String> = on_b.json()["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        b_ids.contains(&b_task) && !b_ids.contains(&a_task),
        "{b_ids:?}"
    );
    // A record id of site A is unknown on site B, with B's own session.
    let a_on_b = ask(other, &other.admin, &a_project).await;
    assert_eq!(
        (a_on_b.status, a_on_b.code().as_str()),
        (404, "graph.not_found"),
        "{}",
        a_on_b.raw
    );
    let b_on_a = ask(&mine, &mine.admin, &b_project).await;
    assert_eq!(
        (b_on_a.status, b_on_a.code().as_str()),
        (404, "graph.not_found"),
        "{}",
        b_on_a.raw
    );
    // A session of site A is no session on site B.
    let crossed = ask(other, &mine.admin, &b_project).await;
    assert!(
        crossed.status == 401 || crossed.status == 403,
        "{}",
        crossed.raw
    );
    stack.finish().await;
}

fn support_id() -> String {
    RecordId::new_v7().to_string()
}
