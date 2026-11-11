//! End-to-end sign-in, permissions and reload over real HTTP against the test PostgreSQL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use spot_postgres::testing::TestDb;
use spot_sessions::SessionPolicy;
use spot_sessions::throttle::ThrottleConfig;
use spot_storage_contract::RecordId;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(text) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn ok(root: &Path, args: &[&str]) -> String {
    let out = run(root, args, None);
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn secret(out: &str) -> String {
    out.trim().rsplit(' ').next().unwrap().to_owned()
}

struct Env {
    root: Root,
    site: String,
    _db: TestDb,
}

impl Env {
    async fn new() -> Option<Env> {
        let db = TestDb::create().await?;
        let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
        let root = Root(std::env::temp_dir().join(format!("spot_cli_e2e_{}", RecordId::new_v7())));
        copy_dir(&repo().join("apps/sample"), &root.0.join("sample"));
        // The sample's `details` comment names the `confidential` group, but the field
        // definition does not carry the property that compiles it; give the test copy the
        // membership the permissions file expects.
        let task = root.0.join("sample/doctypes/task.toml");
        let text = std::fs::read_to_string(&task)
            .unwrap()
            .replace(
                "title_field = \"subject\"",
                "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\ntrace = true",
            )
            .replace(
                "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"",
                "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"\nprotection_group = \"confidential\"",
            );
        std::fs::write(&task, text).unwrap();
        // The owner rule compares `created_by`; the sample's documents do not declare the
        // field yet, so the test app carries it explicitly.
        let entry = root.0.join("sample/doctypes/time_entry.toml");
        let text = std::fs::read_to_string(&entry).unwrap()
            + "\n[[fields]]\nname = \"created_by\"\nlabel = \"Created By\"\ntype = \"text\"\nmax_chars = 64\n";
        std::fs::write(&entry, text).unwrap();
        std::fs::write(
            root.0.join("common.toml"),
            format!(
                "apps = [{:?}, 'sample']\n[server]\nbind = ['127.0.0.1:0']\nshutdown_grace = 2\n[postgres]\nadmin_url = {url:?}\n[trust]\nrequire_signed = false\n",
                repo().join("apps/platform").display().to_string()
            ),
        )
        .unwrap();
        let site = format!(
            "spot-test-{}",
            RecordId::new_v7().to_string().replace('-', "")
        );
        Some(Env {
            root,
            site,
            _db: db,
        })
    }

    /// Creates the site with the platform and the sample app; returns the one-time secret.
    fn site(&self) -> String {
        let out = ok(&self.root.0, &["new-site", &self.site]);
        secret(&out)
    }

    fn user(&self, email: &str, password: &str) -> (String, String) {
        let out = ok(&self.root.0, &["add-user", &self.site, email]);
        let key = secret(&out);
        let out = run(
            &self.root.0,
            &["set-password", &self.site, email, "--password-stdin"],
            Some(&format!("{password}\n")),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (key, email.to_owned())
    }
}

fn fast_options() -> Options {
    Options {
        clock: None,
        login: LoginConfig {
            session_policy: SessionPolicy::DEFAULT,
            throttle: ThrottleConfig {
                backoff_after: 2,
                lockout_after: 3,
                window: Duration::from_secs(60),
                backoff_base: Duration::from_millis(1),
                backoff_multiplier: 1,
                backoff_cap: Duration::from_millis(1),
                lockout_duration: Duration::from_secs(60),
            },
            cache_ttl: Duration::ZERO,
            ..LoginConfig::for_tests()
        },
    }
}

async fn start(
    env: &Env,
    options: Options,
) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(&env.root.0, false, options, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

struct Reply {
    status: u16,
    etag: String,
    body: Json,
}

async fn http(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(bytes).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    let etag = headers
        .lines()
        .find_map(|h| {
            h.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("etag"))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_default();
    let body = if body.is_empty() {
        Json::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|_| panic!("invalid JSON: {body}"))
    };
    Reply { status, etag, body }
}

fn expect(reply: &Reply, status: u16) {
    assert_eq!(reply.status, status, "{}", reply.body);
}

async fn login(addr: SocketAddr, host: &str, user: &str, password: &str) -> Reply {
    http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"user": user, "password": password})),
    )
    .await
}

async fn token_of(addr: SocketAddr, host: &str, user: &str, password: &str) -> String {
    let reply = login(addr, host, user, password).await;
    expect(&reply, 200);
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

async fn user_id(addr: SocketAddr, host: &str, admin: &str, email: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(admin),
        "GET",
        "/api/v1/resource/user?limit=100",
        &[],
        None,
    )
    .await;
    expect(&reply, 200);
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["email"] == email)
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .unwrap()
}

/// The id of a role the platform seeds (as opposed to `role`, which creates one).
async fn seeded_role(addr: SocketAddr, host: &str, admin: &str, name: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(admin),
        "GET",
        "/api/v1/resource/role?limit=100",
        &[],
        None,
    )
    .await;
    expect(&reply, 200);
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["role_name"] == name)
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .unwrap_or_else(|| panic!("the platform seeds no `{name}` role"))
}

async fn role(addr: SocketAddr, host: &str, admin: &str, name: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(admin),
        "POST",
        "/api/v1/resource/role",
        &[],
        Some(json!({"role_name": name, "origin": "reference"})),
    )
    .await;
    expect(&reply, 201);
    reply.body["data"]["id"].as_str().unwrap().to_owned()
}

async fn assign(addr: SocketAddr, host: &str, admin: &str, user: &str, role: &str) {
    let reply = http(
        addr,
        host,
        Some(admin),
        "POST",
        "/api/v1/resource/role_assignment",
        &[],
        Some(json!({
            "user": user, "role": role, "source": "manual", "state": "active",
            "reason": "Assigned by the test operator."
        })),
    )
    .await;
    expect(&reply, 201);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sign_in_permissions_and_the_one_time_secret() {
    let Some(env) = Env::new().await else { return };
    let first_secret = env.site();
    let (_, _) = env.user("member@example.com", "cobalt-harbor-mango-4-Pebble");
    env.user("viewer@example.com", "saffron-meadow-pilot-7-Cedar");
    let (server, addr, stop) = start(&env, fast_options()).await;
    let host = env.site.clone();

    // First sign-in: the secret is the password, and the response says a password is due.
    let first = login(addr, &host, "administrator", &first_secret).await;
    expect(&first, 200);
    assert_eq!(
        first.body["messages"][0]["code"],
        "auth.password_change_required"
    );
    let token = first.body["data"]["token"].as_str().unwrap().to_owned();
    // Until the password is chosen the session reads its own display name, signs out and
    // changes the password, and nothing else (the gate answers 403 with a stable code).
    expect(
        &http(
            addr,
            &host,
            Some(&token),
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await,
        200,
    );
    let early = http(
        addr,
        &host,
        Some(&token),
        "GET",
        "/api/v1/meta/task",
        &[],
        None,
    )
    .await;
    expect(&early, 403);
    assert_eq!(early.body["error"]["code"], "cli.password_change_required");
    let changed = http(
        addr,
        &host,
        Some(&token),
        "POST",
        "/api/v1/auth/password",
        &[],
        Some(json!({"current": first_secret, "new": "violet-lantern-staple-9-Quartz"})),
    )
    .await;
    // The restricted session was replaced by a new one and the old one is revoked.
    expect(&changed, 200);
    assert_ne!(changed.body["data"]["token"], json!(token));
    expect(
        &http(
            addr,
            &host,
            Some(&token),
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await,
        401,
    );
    expect(
        &login(addr, &host, "administrator", &first_secret).await,
        401,
    );
    let admin = token_of(
        addr,
        &host,
        "administrator",
        "violet-lantern-staple-9-Quartz",
    )
    .await;
    let admin_id = first.body["data"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let member_id = user_id(addr, &host, &admin, "member@example.com").await;
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let team_member = role(addr, &host, &admin, "Team Member").await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &member_id, &team_member).await;
    // A link value must name a record the writer may read (an unreadable target is reported
    // as a missing one), and the sample's Task step fills `project_owner`, a User, from the
    // project. The platform's `User Manager` is the only role that reads users, so the member
    // holds it too (the l3-http fixture does the same); it adds no right on the sample DocTypes.
    let user_manager = seeded_role(addr, &host, &admin, "User Manager").await;
    assign(addr, &host, &admin, &member_id, &user_manager).await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;

    // An unauthenticated request never reaches the engine.
    let before = server.engine_ops();
    expect(
        &http(addr, &host, None, "GET", "/api/v1/meta/task", &[], None).await,
        401,
    );
    assert_eq!(server.engine_ops(), before);

    // The Administrator creates a project and one Task with confidential details and one
    // Time Entry of their own.
    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-E2E-1", "title": "E2E", "status": "Active",
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
            "subject": "Admin task", "project": project_id,
            "details": "confidential notes"
        })),
    )
    .await;
    expect(&task, 201);
    let admin_task = task.body["data"]["id"].as_str().unwrap().to_owned();
    let admin_entry = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/time_entry",
        &[],
        Some(json!({
            "task": admin_task, "person": admin_id, "created_by": admin_id,
            "work_date": "2026-10-20", "hours": "1.00"
        })),
    )
    .await;
    expect(&admin_entry, 201);

    // Team Member: sign in, create and update a Task, fail to delete it.
    let member = token_of(
        addr,
        &host,
        "member@example.com",
        "cobalt-harbor-mango-4-Pebble",
    )
    .await;
    let created = http(
        addr,
        &host,
        Some(&member),
        "POST",
        "/api/v1/resource/task",
        &[],
        // The sample's Task step fills `project_owner` from the project; hooks read under the
        // caller's permissions (blueprint ADR-51) and a Team Member holds `project` read.
        Some(json!({"subject": "Member task", "project": project_id})),
    )
    .await;
    expect(&created, 201);
    let task_id = created.body["data"]["id"].as_str().unwrap().to_owned();
    assert!(
        created.body["data"].get("details").is_none(),
        "a field the member may not read must be absent"
    );
    let updated = http(
        addr,
        &host,
        Some(&member),
        "PATCH",
        &format!("/api/v1/resource/task/{task_id}"),
        &[("If-Match", &created.etag)],
        Some(json!({"subject": "Member task edited"})),
    )
    .await;
    expect(&updated, 200);
    let deleted = http(
        addr,
        &host,
        Some(&member),
        "DELETE",
        &format!("/api/v1/resource/task/{task_id}"),
        &[("If-Match", &updated.etag)],
        None,
    )
    .await;
    expect(&deleted, 403);
    assert_eq!(deleted.body["error"]["code"], "permission.denied");
    let hidden = http(
        addr,
        &host,
        Some(&member),
        "GET",
        &format!("/api/v1/resource/task/{admin_task}"),
        &[],
        None,
    )
    .await;
    expect(&hidden, 200);
    assert!(hidden.body["data"].get("details").is_none());

    // Lists return what the caller may read: a Team Member reads projects, so the list holds
    // the one the Administrator made.
    let projects = http(
        addr,
        &host,
        Some(&member),
        "GET",
        "/api/v1/resource/project",
        &[],
        None,
    )
    .await;
    expect(&projects, 200);
    assert_eq!(projects.body["data"].as_array().unwrap().len(), 1);

    // Own Time Entry: create and submit. Another user's: submit is denied.
    let own = http(
        addr,
        &host,
        Some(&member),
        "POST",
        "/api/v1/resource/time_entry",
        &[],
        Some(json!({
            "task": task_id, "person": member_id, "created_by": member_id,
            "work_date": "2026-10-20", "hours": "2.00"
        })),
    )
    .await;
    expect(&own, 201);
    let posted = http(
        addr,
        &host,
        Some(&member),
        "POST",
        &format!(
            "/api/v1/resource/time_entry/{}/post",
            own.body["data"]["id"].as_str().unwrap()
        ),
        &[("If-Match", &own.etag)],
        None,
    )
    .await;
    expect(&posted, 200);
    let admin_entry_id = admin_entry.body["data"]["id"].as_str().unwrap();
    let refused = http(
        addr,
        &host,
        Some(&member),
        "POST",
        &format!("/api/v1/resource/time_entry/{admin_entry_id}/post"),
        &[("If-Match", &admin_entry.etag)],
        None,
    )
    .await;
    expect(&refused, 403);
    assert_eq!(refused.body["error"]["code"], "permission.denied");

    // Viewer reads but cannot write anything.
    let viewer = token_of(
        addr,
        &host,
        "viewer@example.com",
        "saffron-meadow-pilot-7-Cedar",
    )
    .await;
    expect(
        &http(
            addr,
            &host,
            Some(&viewer),
            "GET",
            "/api/v1/resource/task",
            &[],
            None,
        )
        .await,
        200,
    );
    let viewer_write = http(
        addr,
        &host,
        Some(&viewer),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Viewer task", "project": project_id})),
    )
    .await;
    expect(&viewer_write, 403);

    // A second session of the member behaves as the member.
    let member_again = token_of(
        addr,
        &host,
        "member@example.com",
        "cobalt-harbor-mango-4-Pebble",
    )
    .await;
    let key_write = http(
        addr,
        &host,
        Some(&member_again),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Key task", "project": project_id, "project_owner": admin_id})),
    )
    .await;
    expect(&key_write, 201);

    // Logout ends the session immediately.
    expect(
        &http(
            addr,
            &host,
            Some(&viewer),
            "POST",
            "/api/v1/auth/logout",
            &[],
            None,
        )
        .await,
        204,
    );
    expect(
        &http(
            addr,
            &host,
            Some(&viewer),
            "GET",
            "/api/v1/meta/task",
            &[],
            None,
        )
        .await,
        401,
    );

    // Disabling a user stops their live session at once (no session cache).
    let member_etag = http(
        addr,
        &host,
        Some(&admin),
        "GET",
        &format!("/api/v1/resource/user/{member_id}"),
        &[],
        None,
    )
    .await;
    expect(&member_etag, 200);
    let disabled = http(
        addr,
        &host,
        Some(&admin),
        "PATCH",
        &format!("/api/v1/resource/user/{member_id}"),
        &[("If-Match", &member_etag.etag)],
        Some(json!({"state": "disabled"})),
    )
    .await;
    expect(&disabled, 200);
    expect(
        &http(
            addr,
            &host,
            Some(&member),
            "GET",
            "/api/v1/meta/task",
            &[],
            None,
        )
        .await,
        401,
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_sessions_are_refused() {
    let Some(env) = Env::new().await else { return };
    env.site();
    env.user("member@example.com", "cobalt-harbor-mango-4-Pebble");
    let mut options = fast_options();
    let clock = std::sync::Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
    options.clock = Some(clock.clone());
    options.login.session_policy = SessionPolicy {
        idle: Duration::from_secs(3),
        absolute: Duration::from_secs(3600),
        rotate_after: Duration::from_secs(1800),
        max_per_user: 8,
    };
    let (server, addr, stop) = start(&env, options).await;
    let host = env.site.clone();
    let token = token_of(
        addr,
        &host,
        "member@example.com",
        "cobalt-harbor-mango-4-Pebble",
    )
    .await;
    expect(
        &http(
            addr,
            &host,
            Some(&token),
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await,
        200,
    );
    clock.advance(Duration::from_secs(4));
    expect(
        &http(
            addr,
            &host,
            Some(&token),
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await,
        401,
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_passwords_throttle_and_lock() {
    let Some(env) = Env::new().await else { return };
    env.site();
    env.user("member@example.com", "cobalt-harbor-mango-4-Pebble");
    let (server, addr, stop) = start(&env, fast_options()).await;
    let host = env.site.clone();
    let mut statuses = Vec::new();
    let mut last = Json::Null;
    for _ in 0..6 {
        let reply = login(addr, &host, "member@example.com", "not-the-password").await;
        statuses.push(reply.status);
        last = reply.body;
    }
    assert!(
        statuses.contains(&429),
        "no attempt was throttled: {statuses:?}"
    );
    assert_eq!(last["error"]["code"], "api.rate_limited");
    assert!(last["error"]["details"]["retry_after"].is_number());
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_swaps_the_permission_snapshot() {
    let Some(env) = Env::new().await else { return };
    let admin_secret = env.site();
    env.user("viewer@example.com", "saffron-meadow-pilot-7-Cedar");
    let (server, addr, stop) = start(&env, fast_options()).await;
    let host = env.site.clone();
    let first = login(addr, &host, "administrator", &admin_secret).await;
    expect(&first, 200);
    let first_token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let chosen = http(
        addr,
        &host,
        Some(&first_token),
        "POST",
        "/api/v1/auth/password",
        &[],
        Some(json!({"current": admin_secret, "new": "violet-lantern-staple-9-Quartz"})),
    )
    .await;
    expect(&chosen, 200);
    let admin = chosen.body["data"]["token"].as_str().unwrap().to_owned();
    let admin_id = first.body["data"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let viewer_id = user_id(addr, &host, &admin, "viewer@example.com").await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;
    let viewer = token_of(
        addr,
        &host,
        "viewer@example.com",
        "saffron-meadow-pilot-7-Cedar",
    )
    .await;
    let admin_project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-RELOAD-1", "title": "Reload", "status": "Active",
            "owner": admin_id, "currency": "USD", "budget": "1.00"
        })),
    )
    .await;
    expect(&admin_project, 201);
    let project_id = admin_project.body["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let project = http(
        addr,
        &host,
        Some(&viewer),
        "GET",
        "/api/v1/resource/project",
        &[],
        None,
    )
    .await;
    expect(&project, 200);
    let grant = env.root.0.join("sample/permissions/grants.toml");
    let text = std::fs::read_to_string(&grant).unwrap().replace(
        "role = \"Viewer\"\ndoctype = \"task\"\nrights = [\"read\", \"select\"]",
        "role = \"Viewer\"\ndoctype = \"task\"\nrights = [\"read\", \"select\", \"create\"]",
    );
    std::fs::write(&grant, text).unwrap();
    ok(&env.root.0, &["reload", "--site", &env.site]);
    let mut allowed = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let write = http(
            addr,
            &host,
            Some(&viewer),
            "POST",
            "/api/v1/resource/task",
            &[],
            Some(json!({"subject": "after reload", "project": project_id})),
        )
        .await;
        if write.status != 403 {
            allowed = true;
            break;
        }
    }
    assert!(allowed, "the reload never installed the new grant");
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_a_bad_snapshot_answers_503_while_others_serve() {
    let Some(env) = Env::new().await else { return };
    // A catalogue app whose grant names a DocType it does not define: the app itself parses,
    // but the site's merged snapshot cannot be built.
    let badapp = env.root.0.join("badapp");
    std::fs::create_dir_all(badapp.join("doctypes")).unwrap();
    std::fs::create_dir_all(badapp.join("permissions")).unwrap();
    std::fs::write(
        badapp.join("app.toml"),
        "[app]\nname = \"badapp\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        badapp.join("doctypes/widget.toml"),
        "name = \"widget\"\napp = \"badapp\"\n[[fields]]\nname = \"title\"\ntype = \"text\"\n",
    )
    .unwrap();
    std::fs::write(
        badapp.join("permissions/grants.toml"),
        "[[entries]]\nrole = \"Viewer\"\ndoctype = \"missing\"\nrights = [\"read\"]\n",
    )
    .unwrap();
    let common = std::fs::read_to_string(env.root.0.join("common.toml"))
        .unwrap()
        .replace("'sample']", "'sample', 'badapp']");
    std::fs::write(env.root.0.join("common.toml"), common).unwrap();

    let good = format!(
        "spot-good-{}",
        RecordId::new_v7().to_string().replace('-', "")
    );
    let bad = format!(
        "spot-bad-{}",
        RecordId::new_v7().to_string().replace('-', "")
    );
    ok(&env.root.0, &["new-site", &good, "--bare"]);
    ok(
        &env.root.0,
        &["install-app", "sample", "--site", &good, "--apply"],
    );
    ok(&env.root.0, &["new-site", &bad, "--bare"]);
    ok(
        &env.root.0,
        &["install-app", "badapp", "--site", &bad, "--apply"],
    );

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(&env.root.0, false, fast_options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let response = http(addr, &bad, None, "GET", "/api/v1/meta/widget", &[], None).await;
    expect(&response, 503);
    assert_eq!(response.body["error"]["code"], "server.unavailable");
    assert_eq!(
        response.body["error"]["details"]["reason"],
        "site_unavailable"
    );
    assert!(response.body["request_id"].is_string());
    // The other site still serves.
    expect(
        &http(addr, &good, None, "GET", "/api/v1/meta/task", &[], None).await,
        401,
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}
