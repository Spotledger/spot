//! Bulk export and incremental extraction (WP-INT-03.T2) over a real `spot serve` on real
//! PostgreSQL (skipped, with no output, when `SPOT_TEST_PG_URL` is unset).
//!
//! The sample app's grants are copied into the test's own root and given the `export` right
//! for `Project Manager` on `project` and `task`, and for `Team Member` on `task` (who holds no
//! right in the `confidential` group of `task.details`): the repository's grants are not touched.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

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
    fn json(&self) -> Json {
        serde_json::from_str(&self.raw).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    /// The NDJSON lines of the body, each parsed.
    fn lines(&self) -> Vec<Json> {
        self.raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
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

/// `export` added to the rights of the first grant of `role` on `doctype` in `text`.
fn grant_export(text: &str, role: &str, doctype: &str) -> String {
    let head = format!("role = \"{role}\"\ndoctype = \"{doctype}\"\nrights = [");
    assert!(text.contains(&head), "no grant of {role} on {doctype}");
    text.replacen(&head, &format!("{head}\"export\", "), 1)
}

impl Stack {
    async fn start(settings: &str) -> Option<Stack> {
        let env = Env::new().await?;
        let secret = env
            .ok(&["new-site", &env.site])
            .trim()
            .rsplit(' ')
            .next()
            .unwrap()
            .to_owned();
        let grants = env.root.join("sample/permissions/grants.toml");
        let mut text = std::fs::read_to_string(&grants).unwrap();
        text = grant_export(&text, "Project Manager", "project");
        text = grant_export(&text, "Project Manager", "task");
        text = grant_export(&text, "Team Member", "task");
        std::fs::write(&grants, text).unwrap();
        // The sample's `task.details` is meant to sit in the closed `confidential` group but its
        // definition does not say so yet: the test's copy does. `blocked_reason` goes into a
        // group the Team Member may read only masked.
        let task = env.root.join("sample/doctypes/task.toml");
        let mut text = std::fs::read_to_string(&task).unwrap();
        for (field, label, extra) in [
            (
                "details",
                "Details",
                "protection_group = \"confidential\"\n",
            ),
            (
                "blocked_reason",
                "Blocked Reason",
                "protection_group = \"internal_notes\"\nmasking = { last_chars = 4 }\n",
            ),
        ] {
            let head = format!("name = \"{field}\"\nlabel = \"{label}\"\ntype = \"long_text\"\n");
            assert!(text.contains(&head), "no field {field}");
            text = text.replacen(&head, &format!("{head}{extra}"), 1);
        }
        text = text.replacen(
            "title_field = \"subject\"\n",
            "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\n\n[[protection_groups]]\nname = \"internal_notes\"\nlabel = \"Internal notes\"\n",
            1,
        );
        std::fs::write(&task, text).unwrap();
        let groups = env.root.join("sample/permissions/groups.toml");
        let mut text = std::fs::read_to_string(&groups).unwrap();
        text.push_str("\n[[entries]]\nname = \"internal_notes\"\n");
        std::fs::write(&groups, text).unwrap();
        let field_grants = env.root.join("sample/permissions/field_grants.toml");
        let mut text = std::fs::read_to_string(&field_grants).unwrap();
        text.push_str(
            "\n[[entries]]\nrole = \"Team Member\"\ndoctype = \"task\"\ntarget = { group = \"internal_notes\" }\naccess = [\"read_masked\", \"report\"]\n\n[[entries]]\nrole = \"Project Manager\"\ndoctype = \"task\"\ntarget = { group = \"internal_notes\" }\naccess = [\"read\", \"report\"]\n",
        );
        std::fs::write(&field_grants, text).unwrap();
        let toml = env.root.join("common.toml");
        let mut common = std::fs::read_to_string(&toml).unwrap();
        common.push_str(&format!("\n[services.export]\n{settings}\n"));
        std::fs::write(&toml, common).unwrap();
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

    /// A project owned by the Administrator; returns its id.
    async fn project(&self, code: &str, status: &str) -> String {
        let made = self
            .admin(
                "POST",
                "/api/v1/resource/project",
                Some(json!({
                    "code": code,
                    "title": format!("Title {code}"),
                    "status": status,
                    "owner": self.admin_id,
                    "currency": "USD",
                    "budget": "1250.50",
                })),
            )
            .await;
        assert_eq!(made.status, 201, "{}", made.raw);
        made.json()["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn export(&self, token: &str, body: Json) -> Reply {
        self.call(token, "POST", "/api/v1/export", Some(body)).await
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            server.join().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filtered_export_pages_by_cursor_and_matches_the_list() {
    let Some(stack) = Stack::start("rows_per_chunk = 2").await else {
        return;
    };
    let mut expected_active = BTreeSet::new();
    for n in 0..5 {
        let code = format!("PRJ-A{n}");
        stack.project(&code, "Active").await;
        expected_active.insert(code);
    }
    stack.project("PRJ-P0", "Planning").await;
    stack.project("PRJ-P1", "Planning").await;

    // The equivalent list query, as the Administrator.
    let listed = stack
        .admin(
            "POST",
            "/api/v1/resource/project/query",
            Some(
                json!({"filter": {"field": "status", "op": "eq", "value": "Active"}, "limit": 100}),
            ),
        )
        .await;
    assert_eq!(listed.status, 200, "{}", listed.raw);
    let body = listed.json();
    let from_list: BTreeSet<String> = body["data"]
        .as_array()
        .expect("a list of documents")
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(from_list, expected_active);

    // The export holds the same rows, two at a time, with no row twice.
    let manager = stack.session("manager@example.com").await;
    let spec = json!({
        "doctype": "project",
        "columns": ["code", "status", "budget"],
        "filter": {"field": "status", "op": "eq", "value": "Active"},
        "order": [{"field": "code"}],
        "format": "ndjson",
    });
    let mut chunk = stack.export(&manager, spec).await;
    assert_eq!(chunk.status, 200, "{}", chunk.raw);
    assert_eq!(
        chunk.header("content-type").as_deref(),
        Some("application/x-ndjson")
    );
    assert_eq!(
        chunk.header("cache-control").as_deref(),
        Some("private, no-store")
    );
    let watermark = chunk.header("x-export-watermark").expect("a watermark");
    let first_cursor = chunk.header("x-export-cursor").expect("a cursor");
    let mut seen = Vec::new();
    let mut requests = 0;
    loop {
        requests += 1;
        assert!(requests < 20, "the export does not end");
        assert!(
            chunk.lines().len() <= 2,
            "a chunk holds at most rows_per_chunk rows"
        );
        assert_eq!(
            chunk.header("x-export-watermark").as_deref(),
            Some(watermark.as_str())
        );
        for row in chunk.lines() {
            assert_eq!(row["budget"], "1250.50", "decimals are exact strings");
            assert_eq!(row["status"], "Active");
            seen.push(row["code"].as_str().unwrap().to_owned());
        }
        let Some(cursor) = chunk.header("x-export-cursor") else {
            break;
        };
        chunk = stack
            .call(
                &manager,
                "POST",
                "/api/v1/export/next",
                Some(json!({"cursor": cursor})),
            )
            .await;
        assert_eq!(chunk.status, 200, "{}", chunk.raw);
    }
    assert_eq!(
        seen.len(),
        expected_active.len(),
        "no row twice, none missing: {seen:?}"
    );
    assert_eq!(
        seen.iter().cloned().collect::<BTreeSet<_>>(),
        expected_active
    );
    assert_eq!(seen, {
        let mut sorted = seen.clone();
        sorted.sort();
        sorted
    });

    // Resuming from the same cursor returns the same bytes.
    let again = stack
        .call(
            &manager,
            "POST",
            "/api/v1/export/next",
            Some(json!({"cursor": first_cursor})),
        )
        .await;
    let twice = stack
        .call(
            &manager,
            "POST",
            "/api/v1/export/next",
            Some(json!({"cursor": first_cursor})),
        )
        .await;
    assert_eq!(again.raw, twice.raw);

    // A tampered cursor, another caller's cursor and a malformed one are the same answer.
    let mut flipped = first_cursor.clone();
    let last = flipped.pop().unwrap();
    flipped.push(if last == 'A' { 'B' } else { 'A' });
    let member = stack.session("member@example.com").await;
    for (token, cursor) in [
        (&manager, flipped),
        (&member, first_cursor.clone()),
        (&manager, "not a cursor!".to_owned()),
        (&manager, "AAAA".to_owned()),
    ] {
        let refused = stack
            .call(
                token,
                "POST",
                "/api/v1/export/next",
                Some(json!({"cursor": cursor})),
            )
            .await;
        assert_eq!(refused.status, 422, "{}", refused.raw);
        assert_eq!(refused.code(), "bulk.cursor_invalid");
        assert!(
            !refused.raw.contains(&first_cursor),
            "an error never echoes a cursor"
        );
    }
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_export_right_and_the_field_guard_decide_what_leaves() {
    let Some(stack) = Stack::start("rows_per_chunk = 100").await else {
        return;
    };
    let project = stack.project("PRJ-G0", "Active").await;
    let made = stack
        .admin(
            "POST",
            "/api/v1/resource/task",
            Some(json!({
                "subject": "Guarded",
                "project": project,
                "status": "Open",
                "details": "SECRET-DETAILS-TEXT",
                "blocked_reason": "ABCDEFGH1234",
            })),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.raw);

    // No right, no export: denied, not an empty success. The same for a role without it.
    let nobody = stack.session("nobody@example.com").await;
    let member = stack.session("member@example.com").await;
    let manager = stack.session("manager@example.com").await;
    for token in [&nobody, &member] {
        let denied = stack
            .export(token, json!({"doctype": "project", "format": "csv"}))
            .await;
        assert_eq!(denied.status, 403, "{}", denied.raw);
        assert_eq!(denied.code(), "permission.denied");
    }
    let nameless = stack
        .call(
            "not-a-session",
            "POST",
            "/api/v1/export",
            Some(json!({"doctype": "project", "format": "csv"})),
        )
        .await;
    assert_eq!(nameless.status, 401);

    // The manager's CSV: a header and the row, no secret in an error.
    let csv = stack
        .export(
            &manager,
            json!({"doctype": "project", "columns": ["code", "title"], "format": "csv"}),
        )
        .await;
    assert_eq!(csv.status, 200, "{}", csv.raw);
    assert_eq!(
        csv.header("content-type").as_deref(),
        Some("text/csv; charset=utf-8")
    );
    assert!(csv.raw.starts_with("code,title\n"), "{}", csv.raw);
    assert!(csv.raw.contains("PRJ-G0"));

    // The Team Member holds export on `task` but not the `confidential` group: the default
    // leaves `details` out, asking for it is an error naming it, never a dropped column.
    let default = stack
        .export(&member, json!({"doctype": "task", "format": "ndjson"}))
        .await;
    assert_eq!(default.status, 200, "{}", default.raw);
    assert!(
        !default.raw.contains("SECRET-DETAILS-TEXT"),
        "{}",
        default.raw
    );
    assert!(default.lines()[0].get("subject").is_some());
    assert!(default.lines()[0].get("details").is_none());
    let asked = stack
        .export(
            &member,
            json!({"doctype": "task", "columns": ["subject", "details"], "format": "ndjson"}),
        )
        .await;
    assert_eq!(asked.status, 403, "{}", asked.raw);
    assert_eq!(asked.code(), "permission.field_denied");
    assert!(
        asked.raw.contains("details"),
        "the error names the path: {}",
        asked.raw
    );
    assert!(!asked.raw.contains("SECRET-DETAILS-TEXT"));
    // A column that may leave only masked comes out masked; the manager gets the value.
    let masked = stack
        .export(
            &member,
            json!({"doctype": "task", "columns": ["subject", "blocked_reason"], "format": "ndjson"}),
        )
        .await;
    assert_eq!(masked.status, 200, "{}", masked.raw);
    let cell = masked.lines()[0]["blocked_reason"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        cell.ends_with("1234") && !cell.contains("ABCDEFGH"),
        "{cell}"
    );
    // A filter on it is refused the same way (no inference).
    let inferred = stack
        .export(
            &member,
            json!({"doctype": "task", "columns": ["subject"], "format": "ndjson",
                   "filter": {"field": "details", "op": "contains", "value": "SECRET"}}),
        )
        .await;
    assert_eq!(inferred.status, 403, "{}", inferred.raw);
    assert_eq!(inferred.code(), "permission.field_denied");
    // The Project Manager may take it.
    let manager_task = stack
        .export(
            &manager,
            json!({"doctype": "task", "columns": ["subject", "details"], "format": "ndjson"}),
        )
        .await;
    assert_eq!(manager_task.status, 200, "{}", manager_task.raw);
    assert_eq!(manager_task.lines()[0]["details"], "SECRET-DETAILS-TEXT");
    let manager_notes = stack
        .export(
            &manager,
            json!({"doctype": "task", "columns": ["blocked_reason"], "format": "csv"}),
        )
        .await;
    assert_eq!(manager_notes.status, 200, "{}", manager_notes.raw);
    assert!(
        manager_notes.raw.contains("ABCDEFGH1234"),
        "{}",
        manager_notes.raw
    );

    // Unknown things are errors, not empty exports.
    let unknown_column = stack
        .export(
            &manager,
            json!({"doctype": "project", "columns": ["nope"], "format": "csv"}),
        )
        .await;
    assert_eq!(unknown_column.status, 422, "{}", unknown_column.raw);
    assert_eq!(unknown_column.code(), "document.unknown_field");
    // The right comes first: a caller without it cannot tell a missing DocType from a closed one.
    let unknown_doctype = stack
        .export(&manager, json!({"doctype": "nope", "format": "csv"}))
        .await;
    assert_eq!(unknown_doctype.status, 403, "{}", unknown_doctype.raw);
    let unknown_to_admin = stack
        .export(&stack.admin, json!({"doctype": "nope", "format": "csv"}))
        .await;
    assert_eq!(unknown_to_admin.status, 404, "{}", unknown_to_admin.raw);
    let over = stack
        .export(
            &manager,
            json!({"doctype": "project", "format": "csv", "max_rows": 100_000_000_i64}),
        )
        .await;
    assert_eq!(over.status, 400, "{}", over.raw);
    assert_eq!(over.code(), "api.invalid_request");
    assert_eq!(over.json()["error"]["details"]["max_rows"], 10_000_000);

    // Child tables come as one multipart chunk per table.
    let with_children = stack
        .export(
            &manager,
            json!({"doctype": "task", "format": "ndjson", "include_children": true}),
        )
        .await;
    assert_eq!(with_children.status, 200, "{}", with_children.raw);
    assert!(
        with_children
            .header("content-type")
            .unwrap()
            .starts_with("multipart/mixed; boundary=")
    );
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn extraction_reports_changes_and_tombstones_from_a_watermark() {
    let Some(stack) = Stack::start("rows_per_chunk = 100").await else {
        return;
    };
    let kept = stack.project("PRJ-X0", "Active").await;
    let doomed = stack.project("PRJ-X1", "Active").await;
    let manager = stack.session("manager@example.com").await;

    // The watermark of a full export follows it without a gap.
    let full = stack
        .export(
            &manager,
            json!({"doctype": "project", "columns": ["code", "status"], "format": "ndjson"}),
        )
        .await;
    assert_eq!(full.status, 200, "{}", full.raw);
    assert!(full.header("x-export-cursor").is_some() || full.lines().len() == 2);
    let watermark = full.header("x-export-watermark").unwrap();

    let fetched = stack
        .admin("GET", &format!("/api/v1/resource/project/{kept}"), None)
        .await;
    let version = fetched.json()["data"]["version"].as_i64().unwrap();
    let updated = stack
        .admin_update("project", &kept, version, json!({"status": "Done"}))
        .await;
    assert_eq!(updated.status, 200, "{}", updated.raw);
    let added = stack.project("PRJ-X2", "Planning").await;
    let gone = stack
        .admin("GET", &format!("/api/v1/resource/project/{doomed}"), None)
        .await;
    let gone_version = gone.json()["data"]["version"].as_i64().unwrap();
    let removed = stack.admin_delete("project", &doomed, gone_version).await;
    assert_eq!(removed.status, 204, "{}", removed.raw);

    let page = stack
        .call(
            &manager,
            "POST",
            "/api/v1/export/extract",
            Some(json!({"doctype": "project", "columns": ["code", "status"], "watermark": watermark})),
        )
        .await;
    assert_eq!(page.status, 200, "{}", page.raw);
    let data = page.json()["data"].clone();
    let upserts = data["upserts"].as_array().unwrap();
    let ids: BTreeSet<&str> = upserts.iter().map(|u| u["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(kept.as_str()) && ids.contains(added.as_str()),
        "{data}"
    );
    assert!(!ids.contains(doomed.as_str()));
    let kept_row = upserts.iter().find(|u| u["id"] == kept.as_str()).unwrap();
    assert_eq!(kept_row["status"], "Done");
    assert_eq!(kept_row["code"], "PRJ-X0");
    assert!(
        kept_row["version"].as_u64().unwrap() > 1,
        "a version rides on every upsert"
    );
    assert_eq!(
        data["deletes"],
        json!([doomed]),
        "a deleted document is a tombstone, never mere absence"
    );
    assert_eq!(data["has_more"], false);

    // From the new watermark nothing is left, and a forged or foreign one is refused.
    let next = data["new_watermark"].as_str().unwrap().to_owned();
    let drained = stack
        .call(
            &manager,
            "POST",
            "/api/v1/export/extract",
            Some(json!({"doctype": "project", "watermark": next})),
        )
        .await;
    assert_eq!(drained.status, 200, "{}", drained.raw);
    assert_eq!(drained.json()["data"]["upserts"], json!([]));
    assert_eq!(drained.json()["data"]["deletes"], json!([]));
    let member = stack.session("member@example.com").await;
    for (token, doctype, mark) in [
        (&manager, "project", format!("{next}x")),
        (&manager, "task", next.clone()),
        (&member, "project", next.clone()),
    ] {
        let refused = stack
            .call(
                token,
                "POST",
                "/api/v1/export/extract",
                Some(json!({"doctype": doctype, "watermark": mark})),
            )
            .await;
        assert!(
            refused.status == 422 || refused.status == 403,
            "{} {}",
            refused.status,
            refused.raw
        );
        if refused.status == 422 {
            assert_eq!(refused.code(), "bulk.cursor_invalid");
        }
    }
    stack.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_row_budget_answers_429_with_retry_after() {
    let Some(stack) =
        Stack::start("rows_per_chunk = 2\nrows_per_second = 2\nmax_concurrent_per_user = 1").await
    else {
        return;
    };
    for n in 0..3 {
        stack.project(&format!("PRJ-L{n}"), "Active").await;
    }
    let manager = stack.session("manager@example.com").await;
    let spec = json!({"doctype": "project", "columns": ["code"], "format": "ndjson"});
    // Two chunks of two rows inside one second use the budget of two rows per second; the clock
    // may tick between them, so try a few times.
    let mut limited = None;
    for _ in 0..20 {
        let first = stack.export(&manager, spec.clone()).await;
        let second = stack.export(&manager, spec.clone()).await;
        if first.status == 429 || second.status == 429 {
            limited = Some(if first.status == 429 { first } else { second });
            break;
        }
    }
    let limited = limited.expect("the budget was never reached");
    assert_eq!(limited.code(), "bulk.limit");
    let wait: u64 = limited
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    assert!(wait >= 1);
    stack.finish().await;
}

impl Stack {
    async fn admin_update(&self, doctype: &str, id: &str, version: i64, body: Json) -> Reply {
        let etag = format!("\"{version}\"");
        http(
            self.addr,
            &self.host,
            Some(&self.admin),
            "PATCH",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[("If-Match", &etag)],
            Some(body),
        )
        .await
    }

    async fn admin_delete(&self, doctype: &str, id: &str, version: i64) -> Reply {
        let etag = format!("\"{version}\"");
        http(
            self.addr,
            &self.host,
            Some(&self.admin),
            "DELETE",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[("If-Match", &etag)],
            None,
        )
        .await
    }
}
