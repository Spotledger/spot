//! `lineage_trace` over a real `spot serve` on real PostgreSQL (skipped, with no output, when
//! `SPOT_TEST_PG_URL` is unset): chain, batch trace, bill of materials and where-used over the
//! sample app's Project and Task, exact decimals, the permission filter, truncation, an absent or
//! unparsable configuration and two-site isolation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
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
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
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
        raw: payload.to_owned(),
    }
}

/// What a test changes in the stack before it starts.
#[derive(Default)]
struct Cfg<'a> {
    /// The text of the sample app's `lineage.toml` (none: the app ships no configuration).
    lineage: Option<&'a str>,
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
        text.push_str("[services.graph]\nfeed_interval_ms = 50\ndrift_interval_secs = 1\n");
        std::fs::write(&common, text).unwrap();
        if let Some(lineage) = cfg.lineage {
            std::fs::write(env.root.join("sample/lineage.toml"), lineage).unwrap();
        }
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

    /// A task with an exact decimal `estimated_hours` (the quantity field of the test's BOM).
    async fn task_hours(&self, subject: &str, project: &str, hours: &str) -> String {
        self.create(
            "task",
            json!({"subject": subject, "project": project, "estimated_hours": hours}),
        )
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

    async fn lineage(&self, token: &str, body: Json) -> Reply {
        http(
            self.addr,
            &self.host,
            Some(token),
            "POST",
            "/api/v1/lineage",
            Some(body),
        )
        .await
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

/// The chain configuration of the tests' app, in the metadata text form (canonical edge names:
/// `doctype.field`). A Project is the "item" whose components are its Tasks, each Task carrying
/// its per-unit quantity in `estimated_hours`.
const CONFIG: &str = r#"
[[chain]]
doctype = "project"
edges = ["task.project"]

[[chain]]
doctype = "task"
edges = ["task.project"]

[[bom]]
doctype = "project"
edge = "task.project"
qty_field = "estimated_hours"

[[bom]]
doctype = "task"
edge = "task.project"
qty_field = "estimated_hours"
"#;

/// A role that reads Projects and nothing else.
const PROJECT_READER: &str = "[[entries]]\nrole = \"Project Reader\"\ndoctype = \"project\"\nrights = [\"read\", \"select\"]";

fn request(kind: &str, doctype: &str, id: &str) -> Json {
    json!({"kind": kind, "doctype": doctype, "id": id})
}

/// Every node of every level of an answer, with its quantity (when it has one).
fn nodes_of(answer: &Json) -> Vec<(String, Option<String>)> {
    answer["data"]["levels"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|level| level["nodes"].as_array().unwrap().iter())
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_owned(),
                n["quantity"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn has(answer: &Json, id: &str) -> bool {
    nodes_of(answer).iter().any(|(n, _)| n == id)
}

fn quantity_of(answer: &Json, id: &str) -> Option<String> {
    nodes_of(answer)
        .into_iter()
        .find(|(n, _)| n == id)
        .and_then(|(_, q)| q)
}

/// The total of the level at `depth`, as the exact string the route sent.
fn total_at(answer: &Json, depth: u64) -> Option<String> {
    answer["data"]["levels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|level| level["depth"] == depth)
        .and_then(|level| level["quantity_total"].as_str().map(str::to_owned))
}

/// Waits (up to 30 s) until `kind` from `start` answers 200 and `done` holds of the answer.
async fn answer_when(stack: &Stack, token: &str, body: Json, done: impl Fn(&Json) -> bool) -> Json {
    let mut last = Json::Null;
    for _ in 0..60 {
        let reply = stack.lineage(token, body.clone()).await;
        last = reply.json();
        if reply.status == 200 && done(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("the lineage answer never became what the test expects: {last}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_trace_and_where_used_follow_the_links_of_the_configuration() {
    let Some(stack) = Stack::start_with(Cfg {
        lineage: Some(CONFIG),
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("CHAIN").await;
    let one = stack.task("One", &project).await;
    let two = stack.task("Two", &project).await;
    let chain = answer_when(
        &stack,
        &stack.admin,
        request("chain", "project", &project),
        |a| has(a, &one) && has(a, &two),
    )
    .await;
    assert!(
        has(&chain, &project),
        "the start record is level 0: {chain}"
    );
    assert_eq!(chain["data"]["withheld"], false);
    assert_eq!(chain["data"]["truncated"], false);
    // Forward from the project: the tasks that link to it.
    let forward = stack
        .lineage(&stack.admin, request("trace_forward", "project", &project))
        .await;
    assert_eq!(forward.status, 200, "{}", forward.raw);
    assert!(has(&forward.json(), &one) && has(&forward.json(), &two));
    // Back from a task: the project it links to, and not its sibling.
    let back = stack
        .lineage(&stack.admin, request("trace_back", "task", &one))
        .await;
    assert_eq!(back.status, 200, "{}", back.raw);
    assert!(has(&back.json(), &project) && !has(&back.json(), &two));
    // Where used: a task is used by its project.
    let used = stack
        .lineage(&stack.admin, request("where_used", "task", &one))
        .await;
    assert_eq!(used.status, 200, "{}", used.raw);
    assert!(has(&used.json(), &project));
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quantities_are_exact_decimal_strings_end_to_end() {
    let Some(stack) = Stack::start_with(Cfg {
        lineage: Some(CONFIG),
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    // 0.1 + 0.2 is 0.30000000000000004 in a float.
    let small = stack.project("SMALL").await;
    let tenth = stack.task_hours("Tenth", &small, "0.10").await;
    let fifth = stack.task_hours("Fifth", &small, "0.20").await;
    let sum = answer_when(
        &stack,
        &stack.admin,
        request("bom", "project", &small),
        |a| has(a, &tenth) && has(a, &fifth),
    )
    .await;
    assert_eq!(quantity_of(&sum, &tenth).as_deref(), Some("0.1"), "{sum}");
    assert_eq!(quantity_of(&sum, &fifth).as_deref(), Some("0.2"), "{sum}");
    assert_eq!(total_at(&sum, 1).as_deref(), Some("0.3"), "{sum}");
    // 9999999999999999.99 + 0.01 = 10^16: above 2^53, so a float cannot hold either operand.
    let large = stack.project("LARGE").await;
    let big = stack.task_hours("Big", &large, "9999999999999999.99").await;
    let cent = stack.task_hours("Cent", &large, "0.01").await;
    let total = answer_when(
        &stack,
        &stack.admin,
        request("bom", "project", &large),
        |a| has(a, &big) && has(a, &cent),
    )
    .await;
    assert_eq!(
        quantity_of(&total, &big).as_deref(),
        Some("9999999999999999.99"),
        "{total}"
    );
    assert_eq!(
        total_at(&total, 1).as_deref(),
        Some("10000000000000000"),
        "{total}"
    );
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_limit_truncates_the_answer_and_says_so() {
    let Some(stack) = Stack::start_with(Cfg {
        lineage: Some(CONFIG),
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("TRUNC").await;
    for n in 0..5 {
        stack.task(&format!("Task {n}"), &project).await;
    }
    let mut body = request("chain", "project", &project);
    body["limit"] = json!(2);
    let cut = answer_when(&stack, &stack.admin, body, |a| {
        a["data"]["truncated"] == true
    })
    .await;
    assert_eq!(cut["data"]["truncation_reason"], "nodes", "{cut}");
    assert!(nodes_of(&cut).len() <= 3, "{cut}");
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hidden_record_removes_its_paths_and_the_answer_says_withheld() {
    let Some(stack) = Stack::start_with(Cfg {
        lineage: Some(CONFIG),
        grants: PROJECT_READER,
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("HIDDEN").await;
    let task = stack.task("Hidden", &project).await;
    let reader = stack
        .user_with_role("reader@example.com", "Project Reader")
        .await;
    // The Administrator sees the task; the reader (who may read only Projects) does not, and is
    // told that something was withheld.
    answer_when(
        &stack,
        &stack.admin,
        request("chain", "project", &project),
        |a| has(a, &task),
    )
    .await;
    let seen = stack
        .lineage(&reader, request("chain", "project", &project))
        .await;
    assert_eq!(seen.status, 200, "{}", seen.raw);
    assert!(!has(&seen.json(), &task), "{}", seen.raw);
    assert!(has(&seen.json(), &project), "{}", seen.raw);
    assert_eq!(seen.json()["data"]["withheld"], true, "{}", seen.raw);
    // The same through the bill of materials: no quantity of a hidden record.
    let bom = stack
        .lineage(&reader, request("bom", "project", &project))
        .await;
    assert_eq!(bom.status, 200, "{}", bom.raw);
    assert!(!has(&bom.json(), &task), "{}", bom.raw);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_start_is_the_answer_of_an_unknown_id() {
    let Some(stack) = Stack::start_with(Cfg {
        lineage: Some(CONFIG),
        ..Cfg::default()
    })
    .await
    else {
        return;
    };
    let project = stack.project("PRIVATE").await;
    let nobody = stack.user_with_role("nobody@example.com", "Nothing").await;
    let unknown_id = RecordId::new_v7().to_string();
    for kind in ["chain", "trace_forward", "bom"] {
        let denied = stack
            .lineage(&nobody, request(kind, "project", &project))
            .await;
        let unknown = stack
            .lineage(&nobody, request(kind, "project", &unknown_id))
            .await;
        assert_eq!(
            (denied.status, denied.code().as_str()),
            (404, "graph.not_found"),
            "{kind}: {}",
            denied.raw
        );
        // The same status, code and error body for a record that exists and one that does not.
        assert_eq!(denied.status, unknown.status, "{kind}");
        let without_id = |reply: &Reply| {
            let mut error = reply.json()["error"].clone();
            if let Some(map) = error.as_object_mut() {
                map.remove("request_id");
            }
            error
        };
        assert_eq!(without_id(&denied), without_id(&unknown), "{kind}");
    }
    // A forged session is refused before any graph work.
    let forged = stack
        .lineage("not-a-session-token", request("chain", "project", &project))
        .await;
    assert_eq!(forged.status, 401, "{}", forged.raw);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_without_configuration_and_one_with_a_bad_file_serve_no_kind() {
    for text in [None, Some("[[chain]]\ndoctype = 5\n")] {
        let Some(stack) = Stack::start_with(Cfg {
            lineage: text,
            ..Cfg::default()
        })
        .await
        else {
            return;
        };
        let id = RecordId::new_v7().to_string();
        for kind in ["chain", "trace_forward", "trace_back", "bom", "where_used"] {
            let reply = stack
                .lineage(&stack.admin, request(kind, "task", &id))
                .await;
            assert_eq!(
                (reply.status, reply.code().as_str()),
                (422, "lineage.kind_unknown"),
                "{text:?} {kind}: {}",
                reply.raw
            );
            let anonymous = http(
                stack.addr,
                &stack.host,
                None,
                "POST",
                "/api/v1/lineage",
                Some(request(kind, "task", &id)),
            )
            .await;
            assert_eq!(anonymous.status, 401, "{kind}");
        }
        stack.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_of_one_server_share_no_lineage() {
    let Some(stack) = Stack::start_with(Cfg {
        lineage: Some(CONFIG),
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
                "/api/v1/lineage",
                Some(request("chain", "project", &project)),
            )
            .await
        }
    };
    let mine = Tenant {
        host: stack.host.clone(),
        admin: stack.admin.clone(),
        admin_id: stack.admin_id.clone(),
    };
    let mut on_b = ask(other, &other.admin, &b_project).await;
    for _ in 0..60 {
        if on_b.status == 200 && has(&on_b.json(), &b_task) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        on_b = ask(other, &other.admin, &b_project).await;
    }
    assert!(has(&on_b.json(), &b_task), "{}", on_b.raw);
    assert!(!has(&on_b.json(), &a_task), "{}", on_b.raw);
    // A record of site A is unknown on site B, and the other way round.
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
