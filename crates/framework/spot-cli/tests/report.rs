//! Reports (WP-PLT-14.T2) over a real `spot serve` on real PostgreSQL (skipped, with no output,
//! when `SPOT_TEST_PG_URL` is unset): `report_run`, `report_list` and `report_describe` over the
//! report definitions an installed app ships in `<app>/reports/*.toml`.
//!
//! The sample app is copied into the test's own root, which keeps the `hours_by_task` report the
//! app ships, gets three more report files and a protection group on `task.details` (the repository's
//! sample app is not touched). Three users: a Project Manager (admitted to every report, may report on `details`), a Team Member
//! (admitted to the data reports, may not report on `details`) and a user with no role.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::str::FromStr as _;
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

struct Reply {
    status: u16,
    headers: String,
    raw: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
    fn data(&self) -> Json {
        self.json()["data"].clone()
    }
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
    extra: &[(&str, &str)],
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
    for (key, value) in extra {
        head.push_str(&format!("{key}: {value}\r\n"));
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
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        headers: headers.to_owned(),
        raw: payload.to_owned(),
    }
}

const TASK_DETAILS: &str = r#"
name = "task_details"
label = "Task details"
base = "task"
roles = ["Project Manager", "Team Member"]
limit = 100
max_rows = 1000

[[columns]]
field = "subject"

[[columns]]
field = "details"

[[order_by]]
field = "subject"
"#;

const DETAILS_FILTERED: &str = r#"
name = "details_filtered"
label = "Tasks by details"
base = "task"
roles = ["Project Manager", "Team Member"]
limit = 100
max_rows = 1000

[[columns]]
field = "subject"

[filters]
field = "details"
op = "eq"
value = "confidential notes"
"#;

const MANAGERS_ONLY: &str = r#"
name = "managers_only"
label = "Managers only"
base = "project"
roles = ["Project Manager"]
limit = 10
max_rows = 10

[[columns]]
field = "code"
"#;

struct Stack {
    _env: Env,
    addr: SocketAddr,
    host: String,
    admin: String,
    admin_id: String,
    server: Option<Running>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

async fn serve(env: &Env) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(
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
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

impl Stack {
    async fn start(reports: &[(&str, &str)]) -> Option<Stack> {
        let env = Env::new().await?;
        let secret = env
            .ok(&["new-site", &env.site])
            .trim()
            .rsplit(' ')
            .next()
            .unwrap()
            .to_owned();
        // `task.details` is meant to sit in the closed `confidential` group but the sample's
        // definition does not say so yet: the test's copy does. The sample's field grant opens
        // the group to Project Managers only.
        let task = env.root.join("sample/doctypes/task.toml");
        let mut text = std::fs::read_to_string(&task).unwrap();
        let head = "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"\n";
        assert!(text.contains(head), "no field details");
        text = text.replacen(
            head,
            &format!("{head}protection_group = \"confidential\"\n"),
            1,
        );
        text = text.replacen(
            "title_field = \"subject\"\n",
            "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\n",
            1,
        );
        std::fs::write(&task, text).unwrap();
        let dir = env.root.join("sample/reports");
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in reports {
            std::fs::write(dir.join(file), text).unwrap();
        }
        for email in [
            "manager@example.com",
            "member@example.com",
            "nobody@example.com",
        ] {
            spot_cli::users::add_user(&env.root, &env.site, email, None, &[])
                .await
                .unwrap();
            spot_cli::users::set_password(
                &env.root,
                &env.site,
                email,
                &spot_auth::Password::new(PASSWORD),
            )
            .await
            .unwrap();
        }
        Stack::boot(env, secret).await
    }

    async fn boot(env: Env, secret: String) -> Option<Stack> {
        let (server, addr, stop) = serve(&env).await;
        let host = env.site.clone();
        let first = http(
            addr,
            &host,
            None,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(json!({"user": "administrator", "password": secret})),
        )
        .await;
        assert_eq!(first.status, 200, "{}", first.raw);
        let token = first.json()["data"]["token"].as_str().unwrap().to_owned();
        let changed = http(
            addr,
            &host,
            Some(&token),
            "POST",
            "/api/v1/auth/password",
            &[],
            Some(json!({"current": secret, "new": PASSWORD})),
        )
        .await;
        assert_eq!(changed.status, 200, "{}", changed.raw);
        let admin = changed.json()["data"]["token"].as_str().unwrap().to_owned();
        let mut stack = Stack {
            _env: env,
            addr,
            host,
            admin,
            admin_id: String::new(),
            server: Some(server),
            stop: Some(stop),
        };
        stack.admin_id = stack.user_id("administrator").await;
        stack.assign("manager@example.com", "Project Manager").await;
        stack.assign("member@example.com", "Team Member").await;
        Some(stack)
    }
    async fn call(&self, token: &str, method: &str, path: &str, body: Option<Json>) -> Reply {
        http(self.addr, &self.host, Some(token), method, path, &[], body).await
    }

    async fn admin(&self, method: &str, path: &str, body: Option<Json>) -> Reply {
        self.call(&self.admin, method, path, body).await
    }

    async fn user_id(&self, name: &str) -> String {
        let users = self
            .admin("GET", "/api/v1/resource/user?limit=100", None)
            .await
            .json();
        users["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == name || u["user_name"] == name)
            .and_then(|u| u["id"].as_str())
            .unwrap_or_else(|| panic!("no user {name}: {users}"))
            .to_owned()
    }

    async fn assign(&self, email: &str, role: &str) {
        let user = self.user_id(email).await;
        let listed = self
            .admin("GET", "/api/v1/resource/role?limit=100", None)
            .await
            .json();
        let found = listed["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["role_name"] == role)
            .and_then(|r| r["id"].as_str())
            .map(str::to_owned);
        let role_id = match found {
            Some(id) => id,
            None => {
                let made = self
                    .admin(
                        "POST",
                        "/api/v1/resource/role",
                        Some(json!({"role_name": role, "origin": "reference"})),
                    )
                    .await;
                assert_eq!(made.status, 201, "{}", made.raw);
                made.json()["data"]["id"].as_str().unwrap().to_owned()
            }
        };
        let made = self
            .admin(
                "POST",
                "/api/v1/resource/role_assignment",
                Some(json!({"user": user, "role": role_id, "source": "manual", "state": "active", "reason": "export test"})),
            )
            .await;
        assert_eq!(made.status, 201, "{}", made.raw);
    }

    async fn session(&self, email: &str) -> String {
        let reply = http(
            self.addr,
            &self.host,
            None,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(json!({"user": email, "password": PASSWORD})),
        )
        .await;
        assert_eq!(reply.status, 200, "{}", reply.raw);
        reply.json()["data"]["token"].as_str().unwrap().to_owned()
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            server.join().await.unwrap();
        }
    }

    async fn make(&self, path: &str, body: Json) -> Json {
        let made = self.admin("POST", path, Some(body)).await;
        assert_eq!(made.status, 201, "{}", made.raw);
        made.json()["data"].clone()
    }

    /// A task with draft time entries `(date, hours)`; returns its id.
    async fn task_with_hours(
        &self,
        project: &str,
        subject: &str,
        entries: &[(&str, &str)],
    ) -> String {
        let task = self
            .make(
                "/api/v1/resource/task",
                json!({"subject": subject, "project": project, "details": "confidential notes"}),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        for (date, hours) in entries {
            self.make(
                "/api/v1/resource/time_entry",
                json!({
                    "task": task, "person": self.admin_id,
                    "work_date": date, "hours": hours,
                }),
            )
            .await;
        }
        task
    }

    async fn project(&self) -> String {
        self.make(
            "/api/v1/resource/project",
            json!({
                "code": "PRJ-REP-1", "title": "Reports", "status": "Active",
                "owner": self.admin_id, "currency": "USD", "budget": "10.00",
            }),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn names(list: &Json) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect()
}

const ALL: [(&str, &str); 3] = [
    ("task_details.toml", TASK_DETAILS),
    ("details_filtered.toml", DETAILS_FILTERED),
    ("managers_only.toml", MANAGERS_ONLY),
];

#[tokio::test(flavor = "multi_thread")]
async fn the_sample_app_boots_with_its_shipped_report_and_lists_it_for_a_project_manager() {
    // No extra report files: the only definition is the one `apps/sample/reports` ships.
    let Some(stack) = Stack::start(&[]).await else {
        return;
    };
    let manager = stack.session("manager@example.com").await;
    let listed = stack.call(&manager, "GET", "/api/v1/report", None).await;
    assert_eq!(listed.status, 200, "{}", listed.raw);
    assert_eq!(names(&listed.data()), vec!["hours_by_task".to_owned()]);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_list_differs_between_the_manager_and_the_member_and_is_empty_for_a_user_with_no_role()
{
    let Some(stack) = Stack::start(&ALL).await else {
        return;
    };
    let manager = stack.session("manager@example.com").await;
    let member = stack.session("member@example.com").await;
    let nobody = stack.session("nobody@example.com").await;

    let listed = stack.call(&manager, "GET", "/api/v1/report", None).await;
    assert_eq!(listed.status, 200, "{}", listed.raw);
    assert_eq!(
        listed.header("cache-control").as_deref(),
        Some("private, no-store"),
        "a catalogue is per caller"
    );
    // Ordered by label.
    assert_eq!(
        names(&listed.data()),
        [
            "hours_by_task",
            "managers_only",
            "task_details",
            "details_filtered"
        ]
    );
    let first = &listed.data()[0];
    assert_eq!(first["label"], "Hours by task");
    assert_eq!(first["app"], "sample");
    assert_eq!(first["base_doctype"], "time_entry");

    let theirs = stack.call(&member, "GET", "/api/v1/report", None).await;
    assert_eq!(theirs.status, 200, "{}", theirs.raw);
    assert_eq!(
        names(&theirs.data()),
        ["hours_by_task", "task_details", "details_filtered"],
        "a report the member's roles do not admit is not listed"
    );

    let none = stack.call(&nobody, "GET", "/api/v1/report", None).await;
    assert_eq!(none.status, 200, "{}", none.raw);
    assert_eq!(names(&none.data()), Vec::<String>::new());

    let anonymous = http(
        stack.addr,
        &stack.host,
        None,
        "GET",
        "/api/v1/report",
        &[],
        None,
    )
    .await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.raw);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_report_the_roles_do_not_admit_is_not_found_exactly_as_one_that_does_not_exist() {
    let Some(stack) = Stack::start(&ALL).await else {
        return;
    };
    let member = stack.session("member@example.com").await;
    let nobody = stack.session("nobody@example.com").await;
    for token in [&member, &nobody] {
        let hidden = stack
            .call(token, "GET", "/api/v1/report/managers_only", None)
            .await;
        let unknown = stack
            .call(token, "GET", "/api/v1/report/no_such_report", None)
            .await;
        assert_eq!(hidden.status, 404, "{}", hidden.raw);
        assert_eq!(unknown.status, 404, "{}", unknown.raw);
        assert_eq!(hidden.code(), "report.not_found");
        assert_eq!(unknown.code(), "report.not_found");

        let run_hidden = stack
            .call(
                token,
                "POST",
                "/api/v1/report/managers_only/run",
                Some(json!({})),
            )
            .await;
        assert_eq!(run_hidden.status, 404, "{}", run_hidden.raw);
        assert_eq!(run_hidden.code(), "report.not_found");
    }
    // A user with no role is not admitted to the data reports either: denied, not an empty success.
    let denied = stack
        .call(
            &nobody,
            "POST",
            "/api/v1/report/hours_by_task/run",
            Some(json!({"filters": {"from_date": "2026-10-01"}})),
        )
        .await;
    assert_eq!(denied.status, 404, "{}", denied.raw);
    assert_eq!(denied.code(), "report.not_found");
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn describe_states_the_parameters_the_columns_and_the_limit() {
    let Some(stack) = Stack::start(&ALL).await else {
        return;
    };
    let manager = stack.session("manager@example.com").await;
    let described = stack
        .call(&manager, "GET", "/api/v1/report/hours_by_task", None)
        .await;
    assert_eq!(described.status, 200, "{}", described.raw);
    let data = described.data();
    assert_eq!(data["name"], "hours_by_task");
    assert_eq!(data["app"], "sample");
    assert_eq!(data["base_doctype"], "time_entry");
    assert_eq!(data["max_rows"], 1000);
    assert_eq!(
        data["parameters"],
        json!([{"name": "from_date", "label": "From date", "type": "date", "required": true}])
    );
    let columns: Vec<(String, String)> = data["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_owned(),
                c["type"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(columns.len(), 2, "{columns:?}");
    assert_eq!(columns[0].1, "link");
    assert_eq!(columns[1].1, "decimal");
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grouped_report_sums_the_rows_the_filter_keeps_and_honours_the_limit() {
    let Some(stack) = Stack::start(&ALL).await else {
        return;
    };
    let project = stack.project().await;
    let a = stack
        .task_with_hours(
            &project,
            "Task A",
            &[
                ("2026-10-20", "1.50"),
                ("2026-10-21", "2.00"),
                ("2026-09-01", "4.00"),
            ],
        )
        .await;
    let b = stack
        .task_with_hours(&project, "Task B", &[("2026-10-22", "3.00")])
        .await;
    let manager = stack.session("manager@example.com").await;

    let run = stack
        .call(
            &manager,
            "POST",
            "/api/v1/report/hours_by_task/run",
            Some(json!({"filters": {"from_date": "2026-10-01"}})),
        )
        .await;
    assert_eq!(run.status, 200, "{}", run.raw);
    let data = run.data();
    assert_eq!(data["truncated"], false);
    assert_eq!(data["columns"].as_array().unwrap().len(), 2);
    let sums: BTreeMap<String, Decimal> = data["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row[0].as_str().unwrap().to_owned(),
                Decimal::from_str(row[1].as_str().unwrap()).unwrap(),
            )
        })
        .collect();
    let expected = BTreeMap::from([
        (a, Decimal::from_str("3.50").unwrap()),
        (b, Decimal::from_str("3.00").unwrap()),
    ]);
    assert_eq!(sums, expected, "the row before from_date is not summed");

    // The limit cuts the result and says so.
    let cut = stack
        .call(
            &manager,
            "POST",
            "/api/v1/report/hours_by_task/run",
            Some(json!({"filters": {"from_date": "2026-10-01"}, "limit": 1})),
        )
        .await;
    assert_eq!(cut.status, 200, "{}", cut.raw);
    assert_eq!(cut.data()["rows"].as_array().unwrap().len(), 1);
    assert_eq!(cut.data()["truncated"], true);

    // An unknown parameter, and a missing required one, are bad arguments naming no value.
    let unknown = stack
        .call(
            &manager,
            "POST",
            "/api/v1/report/hours_by_task/run",
            Some(json!({"filters": {"from_date": "2026-10-01", "nope": "secret-value"}})),
        )
        .await;
    assert_eq!(unknown.status, 422, "{}", unknown.raw);
    assert_eq!(unknown.code(), "report.bad_argument");
    assert!(!unknown.raw.contains("secret-value"), "{}", unknown.raw);
    let missing = stack
        .call(
            &manager,
            "POST",
            "/api/v1/report/hours_by_task/run",
            Some(json!({})),
        )
        .await;
    assert_eq!(missing.status, 422, "{}", missing.raw);
    assert_eq!(missing.code(), "report.bad_argument");
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_column_the_member_may_not_report_on_is_removed_and_a_filter_naming_it_is_refused() {
    let Some(stack) = Stack::start(&ALL).await else {
        return;
    };
    let project = stack.project().await;
    stack.task_with_hours(&project, "Task A", &[]).await;
    let manager = stack.session("manager@example.com").await;
    let member = stack.session("member@example.com").await;

    // The manager may report on `details`; the member may not: the column is gone from the
    // description and the result, and the result says how many were removed.
    let both = stack
        .call(&manager, "GET", "/api/v1/report/task_details", None)
        .await;
    assert_eq!(both.status, 200, "{}", both.raw);
    assert_eq!(both.data()["columns"].as_array().unwrap().len(), 2);
    let one = stack
        .call(&member, "GET", "/api/v1/report/task_details", None)
        .await;
    assert_eq!(one.status, 200, "{}", one.raw);
    let kept = one.data()["columns"].as_array().unwrap().clone();
    assert_eq!(kept.len(), 1, "{}", one.raw);
    assert_eq!(kept[0]["name"], "subject");
    assert!(
        !one.raw.contains("\"details\"") && !one.raw.contains("confidential"),
        "the removed column is not named: {}",
        one.raw
    );

    let ran = stack
        .call(
            &member,
            "POST",
            "/api/v1/report/task_details/run",
            Some(json!({})),
        )
        .await;
    assert_eq!(ran.status, 200, "{}", ran.raw);
    assert_eq!(ran.data()["columns"].as_array().unwrap().len(), 1);
    for row in ran.data()["rows"].as_array().unwrap() {
        assert_eq!(
            row.as_array().unwrap().len(),
            1,
            "no value of `details` in {row}"
        );
    }
    assert!(!ran.raw.contains("confidential notes"), "{}", ran.raw);
    let messages = ran.json()["messages"].clone();
    assert_eq!(messages[0]["code"], "report.columns_removed", "{}", ran.raw);

    // A filter on a field the member may not report on is refused (no inference), not run.
    let refused = stack
        .call(
            &member,
            "POST",
            "/api/v1/report/details_filtered/run",
            Some(json!({})),
        )
        .await;
    assert_eq!(refused.status, 403, "{}", refused.raw);
    assert_eq!(refused.code(), "permission.field_denied");
    let allowed = stack
        .call(
            &manager,
            "POST",
            "/api/v1/report/details_filtered/run",
            Some(json!({})),
        )
        .await;
    assert_eq!(allowed.status, 200, "{}", allowed.raw);
    assert_eq!(allowed.data()["rows"].as_array().unwrap().len(), 1);
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_definition_that_does_not_compile_stops_the_site_and_names_every_problem() {
    let Some(env) = Env::new().await else {
        return;
    };
    let dir = env.root.join("sample/reports");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.toml"),
        "name = \"bad_base\"\nlabel = \"Bad\"\nbase = \"no_such_doctype\"\nroles = [\"Viewer\"]\n\n[[columns]]\nfield = \"x\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.toml"),
        "name = \"bad_field\"\nlabel = \"Bad\"\nbase = \"task\"\nroles = [\"Viewer\"]\n\n[[columns]]\nfield = \"no_such_field\"\n",
    )
    .unwrap();
    env.ok(&["new-site", &env.site]);
    // The host starts but the site is unavailable (the reason goes to the operator's log, naming
    // every problem; `every_problem_of_every_definition_is_named` in `services/report.rs` proves
    // the naming): it serves nothing, reports included, never half served. The host writes that
    // text to its process stderr (`eprintln!` in `host.rs`), which a test in this process cannot
    // read, so the operator-facing text is not asserted here (see the PR's Open questions).
    let (running, addr, stop) = serve(&env).await;
    for (method, path) in [
        ("GET", "/api/v1/report"),
        ("GET", "/api/v1/report/bad_field"),
        ("POST", "/api/v1/report/bad_field/run"),
        ("GET", "/api/v1/auth/session"),
    ] {
        let reply = http(addr, &env.site, None, method, path, &[], None).await;
        assert_eq!(reply.status, 503, "{method} {path}: {}", reply.raw);
        assert!(reply.raw.contains("server.unavailable"), "{}", reply.raw);
        // The answer is the contract's `server.unavailable` envelope with a request id, and it
        // tells a client nothing about the definitions: no file name, no definition name, no
        // field or doctype name, no path from the host's root.
        let body: Json = serde_json::from_str(&reply.raw)
            .unwrap_or_else(|e| panic!("{method} {path}: not JSON ({e}): {}", reply.raw));
        assert_eq!(body["error"]["code"], "server.unavailable", "{}", reply.raw);
        assert!(
            body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
            "{method} {path}: no request id: {}",
            reply.raw
        );
        let root = env.root.to_string_lossy().into_owned();
        for leak in [
            "a.toml",
            "b.toml",
            "bad_base",
            "bad_field",
            "no_such_field",
            "no_such_doctype",
            "reports",
            "sample",
            root.as_str(),
        ] {
            assert!(
                !reply.raw.contains(leak),
                "{method} {path}: the 503 body leaks `{leak}`: {}",
                reply.raw
            );
        }
        assert!(
            !reply.headers.contains(&root),
            "{method} {path}: a header leaks the root: {}",
            reply.headers
        );
    }
    let _ = stop.send(());
    running.join().await.ok();
}
