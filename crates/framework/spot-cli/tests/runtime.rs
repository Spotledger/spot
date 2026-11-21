//! Runtime behaviour of the hosted services over the real test PostgreSQL: a custom field
//! added while the server runs, two nodes on one metadata store, a shutdown while requests
//! are in flight, and readiness when an optional service is off.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::{Value as Json, json};
use spot_cli::services::Service;
use spot_cli::services::metadata::{Change, LiveMetadata, Settings};
use spot_cli::{Options, config, request_reload, schema};
use spot_core::SystemClock;
use spot_login::LoginConfig;
use spot_meta::{FieldSpec, FieldType};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn options() -> Options {
    Options {
        clock: None,
        login: LoginConfig::for_tests(),
    }
}

struct Reply {
    status: u16,
    body: Json,
    raw: String,
}

/// One request over a fresh connection. `sent` is bumped once the request is on the wire, so
/// a test can stop the server while it is certainly in flight.
async fn request(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Json>,
    sent: Option<&AtomicUsize>,
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
    if let Some(sent) = sent {
        sent.fetch_add(1, Ordering::SeqCst);
    }
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .expect("the response arrives")
        .expect("the connection delivers the whole response");
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw
        .split_once("\r\n\r\n")
        .expect("a complete HTTP response");
    let status = headers
        .split_whitespace()
        .nth(1)
        .expect("a status line")
        .parse()
        .unwrap();
    let body = serde_json::from_str(payload).unwrap_or(Json::Null);
    Reply { status, body, raw }
}

/// Signs in the Administrator with the first-sign-in secret and chooses a password, which is
/// all that session may do; returns the replacement session's token and the user id.
async fn sign_in(addr: SocketAddr, host: &str, secret: &str) -> (String, String) {
    let first = request(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": "administrator", "password": secret})),
        None,
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let first_token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let reply = request(
        addr,
        host,
        Some(&first_token),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": "violet-lantern-staple-9-Quartz"})),
        None,
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.raw);
    (
        reply.body["data"]["token"].as_str().unwrap().to_owned(),
        reply.body["data"]["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

fn secret_of(output: &str) -> String {
    output.trim().rsplit(' ').next().unwrap().to_owned()
}

fn append(root: &Path, text: &str) {
    let path = root.join("common.toml");
    let mut common = std::fs::read_to_string(&path).unwrap();
    common.push_str(text);
    std::fs::write(path, common).unwrap();
}

fn note_field() -> Change {
    let mut spec = FieldSpec::new("custom_note", FieldType::Text);
    spec.max_chars = Some(64);
    Change::AddCustomField {
        doctype: "project".into(),
        spec: Box::new(spec),
        insert_after: None,
    }
}

/// A metadata service the way the host builds one for the site, with `notify` as given.
async fn metadata_node(env: &Env, notify: Arc<dyn Fn() + Send + Sync>) -> Arc<LiveMetadata> {
    let loaded = config::load(&env.root, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == env.site)
        .unwrap()
        .clone();
    let backend = schema::backend(&loaded, &site).await.unwrap();
    let meta = spot_cli::services::metadata::effective(&env.root, &loaded, &site).unwrap();
    let catalog = schema::catalog(&meta).unwrap();
    LiveMetadata::new(Settings {
        root: env.root.clone(),
        loaded,
        site,
        backend: Some(backend),
        catalog: Some(catalog),
        notify,
        clock: Arc::new(SystemClock::default()),
    })
    .await
    .unwrap()
}

fn project(code: &str, owner: &str) -> Json {
    json!({
        "code": code, "title": "Runtime", "status": "Active",
        "owner": owner, "currency": "USD", "budget": "1.00"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_custom_field_added_while_running_appears_in_the_api_and_the_data_survives() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let host = env.site.clone();
    let (token, admin) = sign_in(addr, &host, &secret).await;

    let before = request(
        addr,
        &host,
        Some(&token),
        "POST",
        "/api/v1/resource/project",
        Some(project("PRJ-LIVE-1", &admin)),
        None,
    )
    .await;
    assert_eq!(before.status, 201, "{}", before.raw);
    let before_id = before.body["data"]["id"].as_str().unwrap().to_owned();
    let meta = request(
        addr,
        &host,
        Some(&token),
        "GET",
        "/api/v1/meta/project",
        None,
        None,
    )
    .await;
    assert_eq!(meta.status, 200, "{}", meta.raw);
    assert!(
        !meta.raw.contains("custom_note"),
        "the field does not exist yet"
    );

    // The runtime change goes through the metadata service; its notify is the same reload
    // marker the host's own service raises, so the running host rebuilds without a restart.
    let root = env.root.clone();
    let name = env.site.clone();
    let node = metadata_node(
        &env,
        Arc::new(move || {
            request_reload(&root, &name).unwrap();
        }),
    )
    .await;
    node.apply_change(note_field()).await.unwrap();

    let mut seen = false;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let meta = request(
            addr,
            &host,
            Some(&token),
            "GET",
            "/api/v1/meta/project",
            None,
            None,
        )
        .await;
        if meta.status == 200 && meta.raw.contains("custom_note") {
            seen = true;
            break;
        }
    }
    assert!(seen, "the running server never exposed the new field");

    let mut body = project("PRJ-LIVE-2", &admin);
    body["custom_note"] = json!("written after the change");
    let created = request(
        addr,
        &host,
        Some(&token),
        "POST",
        "/api/v1/resource/project",
        Some(body),
        None,
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.raw);
    let id = created.body["data"]["id"].as_str().unwrap().to_owned();
    let read = request(
        addr,
        &host,
        Some(&token),
        "GET",
        &format!("/api/v1/resource/project/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(read.status, 200, "{}", read.raw);
    assert_eq!(read.body["data"]["custom_note"], "written after the change");

    // The row written before the field existed is still there, untouched.
    let old = request(
        addr,
        &host,
        Some(&token),
        "GET",
        &format!("/api/v1/resource/project/{before_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(old.status, 200, "{}", old.raw);
    assert_eq!(old.body["data"]["code"], "PRJ-LIVE-1");

    stop.send(()).unwrap();
    server.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_on_one_database_see_a_metadata_change_within_a_poll() {
    let Some(env) = Env::new().await else { return };
    append(
        &env.root,
        "\n[services]\nprofile = \"cluster\"\n[services.metadata]\nbus = \"polling\"\npoll_ms = 100\n",
    );
    env.ok(&["new-site", &env.site]);
    let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    let a = metadata_node(&env, noop.clone()).await;
    let b = metadata_node(&env, noop).await;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let runner = {
        let b = b.clone();
        let stopped = stopped.clone();
        tokio::spawn(async move { b.run(stopped).await })
    };
    let first = b.current().version();
    assert_eq!(a.current().version(), first, "both nodes start together");

    let version = a.apply_change(note_field()).await.unwrap();
    // `poll_ms` is 100: a generous bound of twenty polls keeps a loaded runner from flaking
    // while still proving delivery is by the poll and not by a restart.
    let mut adopted = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if b.current().version() == version {
            adopted = true;
            break;
        }
    }
    assert!(adopted, "node B never adopted version {}", version.get());
    assert!(
        b.current()
            .meta()
            .definition("project")
            .is_some_and(|d| d.fields.iter().any(|f| f.name == "custom_note")),
        "A's custom field is in B's metadata"
    );
    stop.send(true).unwrap();
    runner.await.unwrap().unwrap();
}

/// What the kernel says about the server side of every connection to `port`: how many are
/// established (accepted by the kernel, in the backlog or held by the server) and how many of
/// those still have bytes the server has not read. `None` where `/proc/net/tcp` does not exist.
///
/// This is the server's own signal that it has taken a request off the wire: hyper reads the
/// head (and whatever of the body has arrived) from the socket, so a socket with an empty
/// receive queue has been accepted and read; a socket with bytes queued is still waiting for
/// the server, however long ago the client wrote.
fn server_side_sockets(port: u16) -> Option<(usize, usize)> {
    let mut established = 0;
    let mut unread = 0;
    let mut found = false;
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        found = true;
        for line in text.lines().skip(1) {
            let mut cols = line.split_whitespace().skip(1);
            let (Some(local), Some(_remote), Some(state), Some(queues)) =
                (cols.next(), cols.next(), cols.next(), cols.next())
            else {
                continue;
            };
            let local_port = local
                .rsplit(':')
                .next()
                .and_then(|p| u16::from_str_radix(p, 16).ok());
            if local_port != Some(port) || state != "01" {
                continue;
            }
            established += 1;
            let rx = queues
                .split(':')
                .nth(1)
                .and_then(|q| usize::from_str_radix(q, 16).ok())
                .unwrap_or(1);
            if rx > 0 {
                unread += 1;
            }
        }
    }
    found.then_some((established, unread))
}

/// The shutdown grace, in seconds, that the shutdown test gives accepted requests.
const GRACE_SECS: u64 = 60;

/// One client of the shutdown test: the stage it reached, and the answer or what failed.
async fn shutdown_client(
    addr: SocketAddr,
    host: String,
    n: usize,
    started: Arc<AtomicUsize>,
    mut resume: tokio::sync::watch::Receiver<bool>,
) -> Result<String, String> {
    // A request the server has accepted and is still reading: the head and the first half of
    // the body are sent, the rest only after the stop was requested.
    let body = json!({
        "user": format!("nobody{n}@example.com"), "password": "wrong"
    })
    .to_string();
    let (first, rest) = body.split_at(body.len() / 2);
    let head = format!(
        "POST /api/v1/auth/login HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{first}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("client {n}: stage connect: {e}"))?;
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| format!("client {n}: stage write of the head: {e}"))?;
    started.fetch_add(1, Ordering::SeqCst);
    while !*resume.borrow() {
        resume
            .changed()
            .await
            .map_err(|e| format!("client {n}: stage wait for resume: {e}"))?;
    }
    stream
        .write_all(rest.as_bytes())
        .await
        .map_err(|e| format!("client {n}: stage write of the rest of the body: {e}"))?;
    let mut bytes = Vec::new();
    match tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes)).await {
        Err(_) => return Err(format!("client {n}: stage read: no answer in 30 s")),
        Ok(Err(e)) => {
            return Err(format!(
                "client {n}: stage read: {e} after {} bytes",
                bytes.len()
            ));
        }
        Ok(Ok(_)) => {}
    }
    String::from_utf8(bytes).map_err(|e| format!("client {n}: stage read: not UTF-8: {e}"))
}

/// The server's promise on shutdown: a request it has accepted, meaning the head is read and
/// the request is in flight, is served to its end within the shutdown grace, and the answer is
/// complete. Two things are not promised, and the test must not ask for them:
///
/// - A connection the kernel completed but the server has not yet accepted and read has no
///   request in flight; the server may close it. The test stops the server only once it has
///   taken all 24 requests off the wire, which it learns from the kernel's receive queues (a
///   deadline of 30 s, not a fixed sleep).
/// - Work past the grace is cut. The grace is widened here (see `GRACE_SECS`).
///
/// WP-PLT-01.T8 measured the failure this replaces: with two cores and twelve busy loops, 30 of
/// 50 runs failed with an empty answer on some connections; at the stop all 24 were accepted
/// and read, and the drain ended at the 2 s grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_under_load_loses_no_accepted_request() {
    let Some(env) = Env::new().await else { return };
    env.ok(&["new-site", &env.site]);
    // The grace is the time the server gives accepted requests to finish. The harness sets 2 s
    // for the quick shutdown tests; 24 password checks in a debug build on a host with every
    // core busy do not fit in it (measured: the drain ended at the grace, 2.0 to 2.4 s, and
    // the connections it cut had no answer), and `shutdown_does_not_wait_past_the_grace_period`
    // (tests/services.rs) is the test of the grace itself. This test is about requests that
    // were accepted: give them a grace they can meet.
    let common = env.root.join("common.toml");
    let text = std::fs::read_to_string(&common).unwrap();
    let wide = text.replace(
        "shutdown_grace = 2\n",
        &format!("shutdown_grace = {GRACE_SECS}\n"),
    );
    assert_ne!(
        wide, text,
        "the harness config sets the grace this test widens"
    );
    std::fs::write(&common, wide).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let started = Arc::new(AtomicUsize::new(0));
    let (go, resume) = tokio::sync::watch::channel(false);
    let clients = 24;
    let mut tasks = Vec::new();
    for n in 0..clients {
        tasks.push(tokio::spawn(shutdown_client(
            addr,
            env.site.clone(),
            n,
            started.clone(),
            resume.clone(),
        )));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while started.load(Ordering::SeqCst) < clients && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        started.load(Ordering::SeqCst),
        clients,
        "all requests started"
    );
    // Wait until the server has accepted every connection and read what was sent on it.
    let mut sockets = server_side_sockets(addr.port());
    match sockets {
        Some(_) => {
            while tokio::time::Instant::now() < deadline {
                sockets = server_side_sockets(addr.port());
                if sockets == Some((clients, 0)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                sockets,
                Some((clients, 0)),
                "the server accepted and read all {clients} requests within 30 s (established, unread)"
            );
        }
        // No kernel table to read (not Linux): the old budget, which is only as good as the
        // host is idle. Widen `owned_paths` to expose the server's connection counter instead.
        None => tokio::time::sleep(Duration::from_millis(500)).await,
    }
    stop.send(()).unwrap();
    // Coverage, not correctness: give the server time to begin draining, so the clients
    // resume against a server that is shutting down. A slow host makes the test less
    // demanding here, never red.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resumed = std::time::Instant::now();
    go.send(true).unwrap();
    server.join().await.unwrap();
    let drained = resumed.elapsed();
    let mut failures = Vec::new();
    for task in tasks {
        match task.await.unwrap() {
            Err(stage) => failures.push(stage),
            Ok(raw) => {
                let status: u16 = raw
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if status != 401 && status != 429 {
                    failures.push(format!("an accepted request got its answer, got: {raw:?}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {clients} accepted requests lost their answer (server-side sockets before the stop, established and unread: {sockets:?}; the drain took {drained:?} after the clients resumed, against a grace of {GRACE_SECS} s): {failures:#?}",
        failures.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_over_http_reports_a_stopped_optional_service() {
    let Some(env) = Env::new().await else { return };
    // Metrics are the optional service the host can run without: switching them off makes
    // the telemetry check fail, which must degrade the site and not take it out.
    append(&env.root, "\n[services.metrics]\nenabled = false\n");
    env.ok(&["new-site", &env.site]);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let ready = request(
        addr,
        &env.site,
        None,
        "GET",
        "/api/health/ready",
        None,
        None,
    )
    .await;
    assert_eq!(
        ready.status, 200,
        "an optional service does not remove the site from rotation: {}",
        ready.raw
    );
    assert!(
        ready.raw.contains("degraded") && ready.raw.contains("telemetry"),
        "readiness names the degraded service: {}",
        ready.raw
    );
    stop.send(()).unwrap();
    server.join().await.unwrap();
}
