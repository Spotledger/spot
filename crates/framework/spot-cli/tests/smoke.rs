#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value as Json, json};
use spot_postgres::{PgBackend, testing::TestDb};
use spot_storage_contract::{ChangeFeed, FeedPosition, RecordId};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_to_end(&mut bytes),
    )
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
        serde_json::from_str(body).unwrap_or_else(|_| panic!("invalid response JSON: {body}"))
    };
    Reply { status, etag, body }
}
fn expect(r: &Reply, status: u16) {
    assert_eq!(r.status, status, "{}", r.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provisions_and_serves_real_http_lifecycle_with_child_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let root = Root(std::env::temp_dir().join(format!("spot_cli_{}", RecordId::new_v7())));
    std::fs::create_dir_all(root.0.join("app/doctypes")).unwrap();
    std::fs::write(
        root.0.join("app/app.toml"),
        "[app]\nname = \"app\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    std::fs::write(root.0.join("common.toml"), format!("apps = ['app']\n[server]\nbind = ['127.0.0.1:0']\nshutdown_grace = 2\n[postgres]\nadmin_url = {url:?}\n")).unwrap();
    std::fs::write(
        root.0.join("app/doctypes/customer.toml"),
        "name = 'customer'\n[[fields]]\nname = 'title'\ntype = 'text'\nrequired = true\n",
    )
    .unwrap();
    std::fs::write(root.0.join("app/doctypes/invoice.toml"), "name = 'invoice'\nis_posted = true\n[[fields]]\nname = 'customer'\ntype = 'link'\noptions = 'customer'\n[[fields]]\nname = 'locked'\ntype = 'text'\nread_only = true\n[[fields]]\nname = 'items'\ntype = 'table'\noptions = 'invoice_item'\n").unwrap();
    std::fs::write(root.0.join("app/doctypes/invoice_item.toml"), "name = 'invoice_item'\nis_child = true\n[[fields]]\nname = 'text'\ntype = 'text'\nrequired = true\n").unwrap();
    let site = format!(
        "spot-test-{}",
        RecordId::new_v7().to_string().replace('-', "")
    );
    // Exercise the actual product binary and capture its one-time output without logging it.
    let output = tokio::task::spawn_blocking({
        let root = root.0.clone();
        let site = site.clone();
        move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_spot"))
                .args(["new-site", &site, "--root"])
                .arg(root)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "new-site failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let first_secret = stdout
        .trim()
        .strip_prefix(
            "Administrator first sign-in secret (shown once, good for 72 hours, only to choose \
             a password, not an API key): ",
        )
        .unwrap()
        .to_owned();
    assert_eq!(stdout.lines().count(), 1);
    let config =
        std::fs::read_to_string(root.0.join("sites").join(&site).join("site.toml")).unwrap();
    assert!(!config.contains(&first_secret));
    let before = config.clone();
    assert!(spot_cli::new_site(&root.0, &site, "en").await.is_err());
    assert_eq!(
        before,
        std::fs::read_to_string(root.0.join("sites").join(&site).join("site.toml")).unwrap()
    );
    let second_site = format!(
        "spot-test-{}",
        RecordId::new_v7().to_string().replace('-', "")
    );
    let second_secret = spot_cli::new_site(&root.0, &second_site, "en")
        .await
        .unwrap()
        .text();
    let first_path = root.0.join("sites").join(&site).join("site.toml");
    let mut first_config: spot_cli::config::Site = toml::from_str(&config).unwrap();
    first_config.aliases.push("smoke.example".into());
    std::fs::write(first_path, toml::to_string(&first_config).unwrap()).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let handle = spot_cli::start(&root.0, false, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = handle.local_addrs()[0];
    // Sign in with the one-time secret, then replace it with a password.
    let signed_in = http(
        addr,
        &site,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"user": "administrator", "password": &first_secret})),
    )
    .await;
    expect(&signed_in, 200);
    assert_eq!(
        signed_in.body["messages"][0]["code"],
        "auth.password_change_required"
    );
    let first_token = signed_in.body["data"]["token"].as_str().unwrap().to_owned();
    // Until the password is chosen the session can do nothing else.
    let blocked = http(
        addr,
        &site,
        Some(&first_token),
        "POST",
        "/api/v1/resource/customer",
        &[],
        Some(json!({"title":"Too early"})),
    )
    .await;
    expect(&blocked, 403);
    assert_eq!(
        blocked.body["error"]["code"],
        "cli.password_change_required"
    );
    let changed = http(
        addr,
        &site,
        Some(&first_token),
        "POST",
        "/api/v1/auth/password",
        &[],
        Some(json!({
            "current": &first_secret,
            "new": "violet-lantern-staple-9-Quartz"
        })),
    )
    .await;
    // The restricted session is replaced: the answer carries the new session's token.
    expect(&changed, 200);
    let token = changed.body["data"]["token"].as_str().unwrap().to_owned();
    assert_ne!(token, first_token);
    expect(
        &http(
            addr,
            &site,
            Some(&first_token),
            "GET",
            "/api/v1/meta/customer",
            &[],
            None,
        )
        .await,
        401,
    );
    let second_signed_in = http(
        addr,
        &second_site,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"user": "administrator", "password": &second_secret})),
    )
    .await;
    expect(&second_signed_in, 200);
    let second_first = second_signed_in.body["data"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let second_changed = http(
        addr,
        &second_site,
        Some(&second_first),
        "POST",
        "/api/v1/auth/password",
        &[],
        Some(json!({
            "current": &second_secret,
            "new": "violet-lantern-staple-9-Quartz"
        })),
    )
    .await;
    expect(&second_changed, 200);
    let second_token = second_changed.body["data"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = http(
        addr,
        &site,
        None,
        "POST",
        "/api/v1/resource/customer",
        &[],
        Some(json!({"title":"No auth"})),
    )
    .await;
    expect(&r, 401);
    assert_eq!(r.body["error"]["code"], "auth.required");
    let r = http(
        addr,
        &site,
        Some("invalid"),
        "GET",
        "/api/v1/meta/customer",
        &[],
        None,
    )
    .await;
    expect(&r, 401);
    let customer = http(
        addr,
        &site,
        Some(&token),
        "POST",
        "/api/v1/resource/customer",
        &[],
        Some(json!({"title":"Smoke"})),
    )
    .await;
    expect(&customer, 201);
    expect(
        &http(
            addr,
            "smoke.example",
            Some(&token),
            "GET",
            "/api/v1/meta/customer",
            &[],
            None,
        )
        .await,
        200,
    );
    expect(
        &http(
            addr,
            &second_site,
            Some(&token),
            "GET",
            "/api/v1/meta/customer",
            &[],
            None,
        )
        .await,
        401,
    );
    let isolated = http(
        addr,
        &second_site,
        Some(&second_token),
        "GET",
        "/api/v1/resource/customer",
        &[],
        None,
    )
    .await;
    expect(&isolated, 200);
    assert!(isolated.body["data"].as_array().unwrap().is_empty());
    let input = json!({"customer": customer.body["data"]["id"], "items":[{"text":"first"},{"text":"second"}]});
    let created = http(
        addr,
        &site,
        Some(&token),
        "POST",
        "/api/v1/resource/invoice",
        &[("Idempotency-Key", "cli-smoke-create")],
        Some(input.clone()),
    )
    .await;
    expect(&created, 201);
    let replay = http(
        addr,
        &site,
        Some(&token),
        "POST",
        "/api/v1/resource/invoice",
        &[("Idempotency-Key", "cli-smoke-create")],
        Some(input),
    )
    .await;
    expect(&replay, 201);
    assert_eq!(created.body, replay.body);
    let path = format!(
        "/api/v1/resource/invoice/{}",
        created.body["data"]["id"].as_str().unwrap()
    );
    let got = http(addr, &site, Some(&token), "GET", &path, &[], None).await;
    expect(&got, 200);
    assert_eq!(got.body["data"]["items"].as_array().unwrap().len(), 2);
    let mut items = got.body["data"]["items"].clone();
    for row in items.as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("idx");
    }
    items[0]["text"] = json!("changed");
    let updated = http(
        addr,
        &site,
        Some(&token),
        "PATCH",
        &path,
        &[("If-Match", &got.etag)],
        Some(json!({"items":items})),
    )
    .await;
    expect(&updated, 200);
    assert_eq!(updated.body["data"]["items"][0]["text"], "changed");
    let posted = http(
        addr,
        &site,
        Some(&token),
        "POST",
        &format!("{path}/post"),
        &[
            ("If-Match", &updated.etag),
            ("Idempotency-Key", "cli-smoke-post"),
        ],
        None,
    )
    .await;
    expect(&posted, 200);
    let readonly = http(
        addr,
        &site,
        Some(&token),
        "PATCH",
        &path,
        &[("If-Match", &posted.etag)],
        Some(json!({"locked":"forbidden"})),
    )
    .await;
    expect(&readonly, 422);
    expect(
        &http(addr, &site, None, "GET", "/api/health/ready", &[], None).await,
        200,
    );
    let backend = PgBackend::connect(db.config().with_dbname(&site.replace('-', "_")))
        .await
        .unwrap();
    // The journal's `doc_state` images (WP-JRN-02.T2) are not the lifecycle events checked here.
    let events: Vec<_> = backend
        .read_outbox(FeedPosition::START, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.event.topic.as_str() != "doc_state")
        .collect();
    let payloads: Vec<Json> = events
        .iter()
        .map(|e| serde_json::from_str(&e.event.payload).unwrap())
        .collect();
    assert_eq!(payloads.len(), 4, "{payloads:?}");
    assert_eq!(payloads[1]["meta"]["command_id"], "cli-smoke-create");
    assert_eq!(payloads[3]["meta"]["command_id"], "cli-smoke-post");
    let ops: Vec<_> = events.iter().map(|e| e.event.topic.as_str()).collect();
    assert_eq!(
        ops,
        vec!["doc_created", "doc_created", "doc_updated", "doc_posted"]
    );
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle.join())
        .await
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let dev = spot_cli::start(&root.0, true, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = dev.local_addrs()[0];
    expect(
        &http(
            addr,
            &site,
            None,
            "POST",
            "/api/v1/resource/customer",
            &[],
            Some(json!({"title":"Dev Administrator"})),
        )
        .await,
        201,
    );
    expect(
        &http(
            addr,
            &site,
            Some("invalid"),
            "GET",
            "/api/v1/meta/customer",
            &[],
            None,
        )
        .await,
        401,
    );
    stop.send(()).unwrap();
    dev.join().await.unwrap();
    // The test-only spot_test_ database is left for the backend harness's TTL sweep;
    // spot-cli has no SQL or driver dependency, including in this smoke test.
}
