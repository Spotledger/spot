//! Runtime customisation in `spot serve` (WP-PRD-01.T5), over real HTTP and the test
//! PostgreSQL: a DocType made while the server runs is served at once; a plain user may not
//! customise; a second process follows within a poll; a crash between pipeline steps converges
//! on restart; a label override changes the `/meta` ETag.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::{Value as Json, json};
use spot_cli::Options;
use spot_customise::{DocTypeRecord, RecordOp, RecordState, RecordStore, StorageRecordStore};
use spot_login::LoginConfig;
use std::net::SocketAddr;
use std::time::Duration;
use support::{Env, run, text};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
title_field = "subject"
[[fields]]
name = "subject"
type = "text"
required = true
max_chars = 120
[[fields]]
name = "priority"
type = "select"
options = "low\nnormal\nurgent"
default = "normal"
[[fields]]
name = "notes"
type = "table"
options = "ticket_note"
"#;

const TICKET_NOTE: &str = r#"
name = "ticket_note"
is_child = true
[[fields]]
name = "text"
type = "text"
max_chars = 500
"#;

const PAD: &str = r#"
name = "pad"
label = "Pad"
title_field = "text"
[[fields]]
name = "text"
type = "text"
max_chars = 80
"#;

fn options() -> Options {
    Options {
        clock: None,
        login: LoginConfig::for_tests(),
    }
}

struct Reply {
    status: u16,
    etag: String,
    body: Json,
    raw: String,
}

async fn request(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Json>,
) -> Reply {
    request_with(addr, host, token, method, path, &[], body).await
}

async fn request_with(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
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
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
        .await
        .expect("the response arrives")
        .expect("the whole response arrives");
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").expect("an HTTP response");
    let status = headers
        .split_whitespace()
        .nth(1)
        .expect("a status line")
        .parse()
        .unwrap();
    let etag = headers
        .lines()
        .find_map(|h| {
            h.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("etag"))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_default();
    let body = serde_json::from_str(payload).unwrap_or(Json::Null);
    Reply {
        status,
        etag,
        body,
        raw,
    }
}

fn expect(reply: &Reply, status: u16) {
    assert_eq!(reply.status, status, "{}", reply.raw);
}

fn secret_of(output: &str) -> String {
    output.trim().rsplit(' ').next().unwrap().to_owned()
}

async fn token(addr: SocketAddr, host: &str, user: &str, password: &str) -> String {
    let reply = request(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": user, "password": password})),
    )
    .await;
    assert_eq!(reply.status, 200, "sign-in of {user}: {}", reply.raw);
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

/// Creates a user with a password through the CLI. The server reads its credentials when it
/// starts, so this runs before `start`.
fn add_user(env: &Env, email: &str) {
    env.ok(&["add-user", &env.site, email]);
    let out = run(
        &env.root,
        &["set-password", &env.site, email, "--password-stdin"],
        Some(&format!("{PASSWORD}\n")),
    );
    assert!(out.status.success(), "{}", text(&out));
}

const PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";

/// Gives `email` an active assignment to `role` and signs them in.
async fn assign_and_sign_in(
    env: &Env,
    addr: SocketAddr,
    admin: &str,
    email: &str,
    role: Option<&str>,
) -> String {
    if let Some(role) = role {
        let users = request(
            addr,
            &env.site,
            Some(admin),
            "GET",
            "/api/v1/resource/user?limit=100",
            None,
        )
        .await;
        expect(&users, 200);
        let user_id = users.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["email"] == email)
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .unwrap();
        let roles = request(
            addr,
            &env.site,
            Some(admin),
            "GET",
            "/api/v1/resource/role?limit=100",
            None,
        )
        .await;
        expect(&roles, 200);
        let role_id = roles.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["role_name"] == role)
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| panic!("no role {role}: {}", roles.raw));
        let assigned = request(
            addr,
            &env.site,
            Some(admin),
            "POST",
            "/api/v1/resource/role_assignment",
            Some(json!({
                "user": user_id, "role": role_id, "source": "manual", "state": "active",
                "reason": "Assigned by the test operator."
            })),
        )
        .await;
        expect(&assigned, 201);
    }
    token(addr, &env.site, email, PASSWORD).await
}

async fn method(addr: SocketAddr, host: &str, token: &str, function: &str, body: Json) -> Reply {
    request(
        addr,
        host,
        Some(token),
        "POST",
        &format!("/api/v1/method/platform/customise/{function}"),
        Some(json!({ "args": body })),
    )
    .await
}

fn common_cluster(env: &Env) {
    let path = env.root.join("common.toml");
    let mut common = std::fs::read_to_string(&path).unwrap();
    common.push_str(
        "\n[services]\nprofile = \"cluster\"\n[services.metadata]\nbus = \"polling\"\npoll_ms = 100\n",
    );
    std::fs::write(path, common).unwrap();
}

const ADMIN_PASSWORD: &str = "violet-lantern-staple-9-Quartz";

/// Signs in the Administrator and returns a full session. The first-sign-in secret only lets
/// the Administrator choose a password, so the first call does that; later calls (and other
/// nodes that already know the password) sign in with it.
async fn administrator(addr: SocketAddr, host: &str, secret: &str) -> String {
    let chosen = request(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": "administrator", "password": ADMIN_PASSWORD})),
    )
    .await;
    if chosen.status == 200 {
        return chosen.body["data"]["token"].as_str().unwrap().to_owned();
    }
    let first = token(addr, host, "administrator", secret).await;
    let changed = request(
        addr,
        host,
        Some(&first),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": ADMIN_PASSWORD})),
    )
    .await;
    expect(&changed, 200);
    changed.body["data"]["token"].as_str().unwrap().to_owned()
}

struct Node {
    server: spot_cli::Running,
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
}

async fn start(env: &Env) -> Node {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    Node { server, addr, stop }
}

impl Node {
    async fn finish(self) {
        let _ = self.stop.send(());
        self.server.join().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_doctype_made_while_the_server_runs_is_served_with_no_restart() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    add_user(&env, "manager@example.com");
    add_user(&env, "plain@example.com");
    let node = start(&env).await;
    let (addr, host) = (node.addr, env.site.clone());
    let admin = administrator(addr, &host, &secret).await;
    let manager = assign_and_sign_in(
        &env,
        addr,
        &admin,
        "manager@example.com",
        Some("System Manager"),
    )
    .await;
    let plain = assign_and_sign_in(&env, addr, &admin, "plain@example.com", None).await;

    // The DocType does not exist yet.
    let missing = request(
        addr,
        &host,
        Some(&manager),
        "GET",
        "/api/v1/meta/ticket",
        None,
    )
    .await;
    expect(&missing, 404);

    // A System Manager creates it (with its child table) in one change.
    let created = method(
        addr,
        &host,
        &manager,
        "create_doctype",
        json!({"definitions": [TICKET, TICKET_NOTE]}),
    )
    .await;
    expect(&created, 200);
    assert!(created.body["data"]["version"].as_u64().unwrap() > 0);

    // The very next requests: the metadata, then full CRUD, with no restart and no reload.
    let meta = request(
        addr,
        &host,
        Some(&manager),
        "GET",
        "/api/v1/meta/ticket",
        None,
    )
    .await;
    expect(&meta, 200);
    assert_eq!(meta.body["data"]["doctype"], "ticket");
    let row = request(
        addr,
        &host,
        Some(&manager),
        "POST",
        "/api/v1/resource/ticket",
        Some(json!({"subject": "Printer is on fire", "notes": [{"text": "first note"}]})),
    )
    .await;
    expect(&row, 201);
    let id = row.body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        row.body["data"]["priority"], "normal",
        "the default applies"
    );
    let got = request(
        addr,
        &host,
        Some(&manager),
        "GET",
        &format!("/api/v1/resource/ticket/{id}"),
        None,
    )
    .await;
    expect(&got, 200);
    assert_eq!(got.body["data"]["notes"][0]["text"], "first note");
    let patched = request_with(
        addr,
        &host,
        Some(&manager),
        "PATCH",
        &format!("/api/v1/resource/ticket/{id}"),
        &[("If-Match", &got.etag)],
        Some(json!({"priority": "urgent"})),
    )
    .await;
    assert!(
        patched.status == 200 || patched.status == 204,
        "{}",
        patched.raw
    );
    let listed = request(
        addr,
        &host,
        Some(&manager),
        "GET",
        "/api/v1/resource/ticket?limit=10",
        None,
    )
    .await;
    expect(&listed, 200);
    assert_eq!(listed.body["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed.body["data"][0]["priority"], "urgent");

    // Readiness stays green with customised metadata.
    let ready = request(addr, &host, None, "GET", "/api/health/ready", None).await;
    expect(&ready, 200);

    // The permission summary knows the new DocType too.
    let summary = request(
        addr,
        &host,
        Some(&manager),
        "GET",
        "/api/v1/auth/permissions?doctypes=ticket",
        None,
    )
    .await;
    expect(&summary, 200);

    // A custom field on a standard DocType and on the new one, then a row that uses it.
    let field = method(
        addr,
        &host,
        &manager,
        "add_custom_field",
        json!({"doctype": "ticket", "field": {"name": "custom_site", "type": "text", "max_chars": 40}}),
    )
    .await;
    expect(&field, 200);
    let with_field = request(
        addr,
        &host,
        Some(&manager),
        "POST",
        "/api/v1/resource/ticket",
        Some(json!({"subject": "Second", "custom_site": "Karachi"})),
    )
    .await;
    expect(&with_field, 201);
    assert_eq!(with_field.body["data"]["custom_site"], "Karachi");
    let second_id = with_field.body["data"]["id"].as_str().unwrap().to_owned();
    let deleted = request_with(
        addr,
        &host,
        Some(&manager),
        "DELETE",
        &format!("/api/v1/resource/ticket/{second_id}"),
        &[("If-Match", &with_field.etag)],
        None,
    )
    .await;
    assert!(
        deleted.status == 200 || deleted.status == 204,
        "{}",
        deleted.raw
    );

    // The listing shows what was made, applied.
    let entries = method_get(addr, &host, &manager, "list").await;
    expect(&entries, 200);
    let kinds: Vec<_> = entries.body["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["kind"].as_str().unwrap(), e["state"].as_str().unwrap()))
        .collect();
    assert!(kinds.contains(&("custom_doctype", "applied")), "{kinds:?}");
    assert!(kinds.contains(&("custom_field", "applied")), "{kinds:?}");

    // A plain user is refused on every customise method, and cannot read the new DocType.
    for (function, body) in [
        ("create_doctype", json!({"definition": TICKET})),
        (
            "update_doctype",
            json!({"name": "ticket", "definition": TICKET}),
        ),
        (
            "add_custom_field",
            json!({"doctype": "ticket", "field": {"name": "custom_x", "type": "text"}}),
        ),
        (
            "remove_custom_field",
            json!({"doctype": "ticket", "name": "custom_site"}),
        ),
        (
            "set_property",
            json!({"doctype": "ticket", "property": "label", "value": "X"}),
        ),
        (
            "clear_property",
            json!({"doctype": "ticket", "property": "label"}),
        ),
    ] {
        let reply = method(addr, &host, &plain, function, body).await;
        expect(&reply, 403);
    }
    expect(&method_get(addr, &host, &plain, "list").await, 403);
    let denied = request(
        addr,
        &host,
        Some(&plain),
        "GET",
        "/api/v1/resource/ticket?limit=10",
        None,
    )
    .await;
    assert!(
        denied.status == 403 || denied.status == 200 && denied.body["data"] == json!([]),
        "default closed: {}",
        denied.raw
    );

    // Every problem comes back at once, with its path.
    let bad = method(
        addr,
        &host,
        &manager,
        "add_custom_field",
        json!({"doctype": "ticket", "field": {"name": "subject", "type": "text"}}),
    )
    .await;
    expect(&bad, 422);
    assert!(
        !bad.body["error"]["fields"].as_array().unwrap().is_empty(),
        "{}",
        bad.raw
    );

    node.finish().await;
}

async fn method_get(addr: SocketAddr, host: &str, token: &str, function: &str) -> Reply {
    request(
        addr,
        host,
        Some(token),
        "GET",
        &format!("/api/v1/method/platform/customise/{function}"),
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_override_changes_the_meta_etag() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let node = start(&env).await;
    let (addr, host) = (node.addr, env.site.clone());
    let admin = administrator(addr, &host, &secret).await;

    let before = request(
        addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/meta/project",
        None,
    )
    .await;
    expect(&before, 200);
    assert!(!before.etag.is_empty());
    let set = method(
        addr,
        &host,
        &admin,
        "set_property",
        json!({"doctype": "project", "field": "title", "property": "label", "value": "Project name"}),
    )
    .await;
    expect(&set, 200);
    let after = request(
        addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/meta/project",
        None,
    )
    .await;
    expect(&after, 200);
    assert_ne!(before.etag, after.etag, "the ETag follows the metadata");
    assert!(after.raw.contains("Project name"), "{}", after.raw);

    // Clearing it restores the standard label.
    let cleared = method(
        addr,
        &host,
        &admin,
        "clear_property",
        json!({"doctype": "project", "field": "title", "property": "label"}),
    )
    .await;
    expect(&cleared, 200);
    let restored = request(
        addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/meta/project",
        None,
    )
    .await;
    assert!(!restored.raw.contains("Project name"), "{}", restored.raw);
    node.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_process_serves_the_new_doctype_within_a_poll() {
    let Some(env) = Env::new().await else { return };
    common_cluster(&env);
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let a = start(&env).await;
    let b = start(&env).await;
    let host = env.site.clone();
    let admin_a = administrator(a.addr, &host, &secret).await;

    let created = method(
        a.addr,
        &host,
        &admin_a,
        "create_doctype",
        json!({"definitions": [TICKET, TICKET_NOTE]}),
    )
    .await;
    expect(&created, 200);

    // B learns of it by polling (100 ms): a generous bound keeps a loaded runner from flaking.
    let admin_b = administrator(b.addr, &host, &secret).await;
    let mut served = false;
    for _ in 0..80 {
        let meta = request(
            b.addr,
            &host,
            Some(&admin_b),
            "GET",
            "/api/v1/meta/ticket",
            None,
        )
        .await;
        if meta.status == 200 {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(served, "node B never served the new DocType");
    let row = request(
        b.addr,
        &host,
        Some(&admin_b),
        "POST",
        "/api/v1/resource/ticket",
        Some(json!({"subject": "Written on B"})),
    )
    .await;
    expect(&row, 201);
    let id = row.body["data"]["id"].as_str().unwrap().to_owned();
    // ... and A reads what B wrote.
    let seen = request(
        a.addr,
        &host,
        Some(&admin_a),
        "GET",
        &format!("/api/v1/resource/ticket/{id}"),
        None,
    )
    .await;
    expect(&seen, 200);

    a.finish().await;
    b.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_serves_what_was_made_and_converges_a_half_done_change() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let first = start(&env).await;
    let (addr, host) = (first.addr, env.site.clone());
    let admin = administrator(addr, &host, &secret).await;
    expect(
        &method(
            addr,
            &host,
            &admin,
            "create_doctype",
            json!({"definitions": [TICKET, TICKET_NOTE]}),
        )
        .await,
        200,
    );
    first.finish().await;

    // A process killed between two steps leaves the intent record `pending`. Write it the way
    // the pipeline's persist step does (nothing applied to the schema, no snapshot swapped)
    // and start again: the start-up recovery finishes it before the site accepts a request.
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == env.site)
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
    let meta = spot_cli::services::metadata::effective(&env.root, &loaded, &site).unwrap();
    let catalog = std::sync::Arc::new(spot_cli::schema::catalog(&meta).unwrap());
    let records = StorageRecordStore::new(std::sync::Arc::new(backend), catalog);
    records
        .commit(vec![RecordOp::PutDocType(DocTypeRecord {
            name: "pad".into(),
            module: "custom".into(),
            definition: PAD.into(),
            state: RecordState::Pending,
            reason: None,
            intent: 99,
            actor: "crashed-node".into(),
            prior: None,
        })])
        .await
        .unwrap();

    let second = start(&env).await;
    let admin = administrator(second.addr, &host, &secret).await;
    let meta = request(
        second.addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/meta/ticket",
        None,
    )
    .await;
    expect(&meta, 200);
    let row = request(
        second.addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/ticket",
        Some(json!({"subject": "After the restart"})),
    )
    .await;
    expect(&row, 201);
    // The half-done change was finished: its DocType is served and usable.
    let pad = request(
        second.addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/pad",
        Some(json!({"text": "recovered"})),
    )
    .await;
    expect(&pad, 201);
    let entries = method_get(second.addr, &host, &admin, "list").await;
    expect(&entries, 200);
    assert!(
        entries.body["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "pad" && e["state"] == "applied"),
        "{}",
        entries.raw
    );
    second.finish().await;
}
