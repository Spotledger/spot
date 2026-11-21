//! The durable job store, the coordinator and mail sending, across real processes and a real
//! PostgreSQL: kill -9 of `spot serve` in the middle of a job, a retry against a failing SMTP
//! stub, a restart that must not send twice, and two nodes of one site.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use spot_coordinator::Coordinator as _;
use spot_jobs::JobStore as _;
use spot_mail_queue::{QueuedMailbox, QueuedMessage, enqueue_mail};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------------------------
// An SMTP server that can be told to hang, to fail the end of DATA, or to accept.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Accept,
    /// Answer the end of DATA with a 4xx reply this many more times, then accept.
    Fail(usize),
    /// Greet, then never answer again.
    Hang,
}

struct Stub {
    port: u16,
    mode: Arc<Mutex<Mode>>,
    connections: Arc<AtomicUsize>,
    delivered: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    async fn start(mode: Mode) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stub = Stub {
            port,
            mode: Arc::new(Mutex::new(mode)),
            connections: Arc::new(AtomicUsize::new(0)),
            delivered: Arc::new(Mutex::new(Vec::new())),
        };
        let (mode, connections, delivered) = (
            stub.mode.clone(),
            stub.connections.clone(),
            stub.delivered.clone(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                connections.fetch_add(1, Ordering::SeqCst);
                let (mode, delivered) = (mode.clone(), delivered.clone());
                tokio::spawn(async move { serve(stream, mode, delivered).await });
            }
        });
        stub
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn delivered(&self) -> Vec<String> {
        self.delivered.lock().unwrap().clone()
    }
}

async fn serve(
    stream: tokio::net::TcpStream,
    mode: Arc<Mutex<Mode>>,
    delivered: Arc<Mutex<Vec<String>>>,
) {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    if write.write_all(b"220 stub ESMTP\r\n").await.is_err() {
        return;
    }
    if *mode.lock().unwrap() == Mode::Hang {
        // Hold the connection open and say nothing more.
        let mut sink = String::new();
        let _ = read.read_line(&mut sink).await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
        return;
    }
    let mut line = String::new();
    let mut in_data = false;
    let mut body = String::new();
    loop {
        line.clear();
        match read.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if in_data {
            if line == ".\r\n" {
                in_data = false;
                let refuse = {
                    let mut mode = mode.lock().unwrap();
                    match *mode {
                        Mode::Fail(n) if n > 0 => {
                            *mode = Mode::Fail(n - 1);
                            true
                        }
                        _ => false,
                    }
                };
                if refuse {
                    let _ = write.write_all(b"451 4.3.0 try again later\r\n").await;
                } else {
                    let id = body
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Message-ID: ")
                                .or_else(|| l.strip_prefix("Message-Id: "))
                        })
                        .unwrap_or("?")
                        .trim()
                        .to_owned();
                    delivered.lock().unwrap().push(id);
                    let _ = write.write_all(b"250 2.0.0 queued\r\n").await;
                }
                body.clear();
            } else {
                body.push_str(&line);
            }
            continue;
        }
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            b"250-stub\r\n250 8BITMIME\r\n"
        } else if upper.starts_with("HELO")
            || upper.starts_with("MAIL")
            || upper.starts_with("RCPT")
            || upper.starts_with("RSET")
            || upper.starts_with("NOOP")
        {
            b"250 ok\r\n"
        } else if upper.starts_with("DATA") {
            in_data = true;
            b"354 go ahead\r\n"
        } else if upper.starts_with("QUIT") {
            let _ = write.write_all(b"221 bye\r\n").await;
            return;
        } else {
            b"502 not implemented\r\n"
        };
        if write.write_all(reply).await.is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// `spot serve` as a real process.

struct Node {
    child: Child,
    log: PathBuf,
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

impl Node {
    async fn spawn(env: &Env, services: &str, label: &str) -> Node {
        let port = free_port();
        env.write_common(&["'sample'"], &format!("{}\n{services}", env.services));
        // The bind address is the only per-node setting: rewrite it in common.toml.
        let path = env.root.0.join("common.toml");
        let text = std::fs::read_to_string(&path).unwrap().replace(
            "bind = ['127.0.0.1:0']",
            &format!("bind = ['127.0.0.1:{port}']"),
        );
        std::fs::write(&path, text).unwrap();
        let log = env.root.0.join(format!("{label}.log"));
        let out = std::fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_spot"))
            .arg("--root")
            .arg(&env.root.0)
            .arg("serve")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut node = Node { child, log };
        let ready = until(60, || async { ready(addr, &env.site).await }).await;
        assert!(ready, "{label} did not become ready:\n{}", node.log());
        node.child.try_wait().unwrap();
        node
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// SIGKILL: no shutdown, no release, no flush.
    fn kill9(&mut self) {
        self.child.kill().unwrap();
        let _ = self.child.wait();
    }

    fn stop(&mut self) {
        self.kill9();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether the host at `addr` answers `/api/health/ready` for `site` with 200 (a refused
/// connection is just "not yet").
async fn ready(addr: SocketAddr, site: &str) -> bool {
    let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await else {
        return false;
    };
    let request =
        format!("GET /api/health/ready HTTP/1.1\r\nHost: {site}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut text = String::new();
    let _ = tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut text).await;
    text.starts_with("HTTP/1.1 200")
}

async fn until<F, Fut>(seconds: u64, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// The site's job store, the way another process (or an app's hook) reaches it.

struct Db {
    store: spot_jobs_pg::PgJobStore,
    backend: spot_postgres::PgBackend,
}

async fn db(env: &Env) -> Db {
    let site: toml::Table = std::fs::read_to_string(env.site_dir().join("site.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let name = site["database"].as_str().unwrap().to_owned();
    let config = spot_postgres::PgConfig::from_url(&env.url)
        .unwrap()
        .with_dbname(&name);
    let backend = spot_postgres::PgBackend::connect(config).await.unwrap();
    let store = spot_jobs_pg::PgJobStore::new(backend.pool());
    store.prepare().await.unwrap();
    Db { store, backend }
}

async fn send(db: &Db, env: &Env, message_id: &str, keyed: bool) {
    let mut message = QueuedMessage::new(
        QueuedMailbox::address("noreply@example.com"),
        format!("subject {message_id}"),
    );
    message
        .to
        .push(QueuedMailbox::address("someone@example.com"));
    message.text = Some("hello".to_owned());
    let site = spot_core::SiteId::new(&env.site).unwrap();
    let mut job = enqueue_mail(
        site,
        &message,
        "default",
        message_id,
        time::OffsetDateTime::now_utc(),
    )
    .unwrap();
    if !keyed {
        job.idempotency_key = None;
    }
    db.store.insert_if_new(job).await.unwrap();
}

fn smtp(port: u16, sent_log: &str) -> String {
    // An empty `sent_log` leaves the setting out: the default.
    let sent_log = if sent_log.is_empty() {
        String::new()
    } else {
        format!("sent_log = \"{sent_log}\"\n")
    };
    format!(
        "[services.mail]\nsink = \"smtp\"\n{sent_log}[services.mail.smtp]\nhost = \"127.0.0.1\"\nport = {port}\nsecurity = \"plain\"\nfrom = \"noreply@example.com\"\n"
    )
}

// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_killed_mid_run_completes_exactly_once_after_a_restart() {
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    let stub = Stub::start(Mode::Hang).await;
    let services = smtp(stub.port, "postgres");
    let mut node = Node::spawn(&env, &services, "first").await;
    let db = db(&env).await;
    send(&db, &env, "kill-1@example.com", true).await;
    // The worker claimed the job and is talking to a server that never answers.
    assert!(
        until(30, || async {
            stub.connections.load(Ordering::SeqCst) >= 1
        })
        .await,
        "the job never started:\n{}",
        node.log()
    );
    node.kill9();

    // A server that works, and a new process: the claim lapses, the job returns to the queue
    // and runs again.
    stub.set(Mode::Accept);
    let node = Node::spawn(&env, &services, "second").await;
    assert!(
        until(90, || async { !stub.delivered().is_empty() }).await,
        "the job was lost:\n{}",
        node.log()
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(stub.delivered().len(), 1, "{:?}", stub.delivered());
    drop(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_send_is_retried_and_a_restart_never_sends_a_delivered_message_again() {
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    let stub = Stub::start(Mode::Fail(1)).await;
    let services = smtp(stub.port, "postgres");
    let mut node = Node::spawn(&env, &services, "first").await;
    let db = db(&env).await;
    send(&db, &env, "retry-1@example.com", true).await;
    // The first attempt is refused with a 4xx, the retry is accepted.
    assert!(
        until(60, || async { !stub.delivered().is_empty() }).await,
        "the failed send was never retried:\n{}",
        node.log()
    );
    assert_eq!(stub.delivered().len(), 1);
    // The server's acceptance and the sent log's record are two steps: a kill between them is
    // the at-least-once window no log can close. Wait for the record before stopping.
    let log = spot_mail_queue_pg::PgSentLog::new(db.backend.pool());
    let site = spot_core::SiteId::new(&env.site).unwrap();
    assert!(
        until(30, || async {
            spot_mail_queue::SentLog::is_sent(&log, &site, "retry-1@example.com")
                .await
                .unwrap_or(false)
        })
        .await,
        "the accepted message was never recorded:\n{}",
        node.log()
    );
    node.stop();

    // After a restart the same message id arrives again (as a retry of a job whose completion
    // was lost would): the durable sent log knows it was accepted, and nothing is sent.
    let node = Node::spawn(&env, &services, "second").await;
    send(&db, &env, "retry-1@example.com", false).await;
    // A different message proves the worker is running, and ran after the duplicate.
    send(&db, &env, "retry-2@example.com", true).await;
    assert!(
        until(60, || async { stub.delivered().len() >= 2 }).await,
        "the worker did not run after the restart:\n{}",
        node.log()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    let delivered = stub.delivered();
    assert_eq!(
        delivered.len(),
        2,
        "a delivered message was sent again: {delivered:?}"
    );
    assert_ne!(delivered[0], delivered[1]);
    drop(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn by_default_a_restart_never_sends_an_accepted_message_again_and_migrate_still_succeeds() {
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    let stub = Stub::start(Mode::Accept).await;
    // No `sent_log` setting at all: the job store is PostgreSQL, so the sent log is too.
    let services = smtp(stub.port, "");
    let mut node = Node::spawn(&env, &services, "first").await;
    let db = db(&env).await;
    send(&db, &env, "default-1@example.com", true).await;
    assert!(
        until(60, || async { !stub.delivered().is_empty() }).await,
        "the message was never sent:\n{}",
        node.log()
    );
    let log = spot_mail_queue_pg::PgSentLog::new(db.backend.pool());
    let site = spot_core::SiteId::new(&env.site).unwrap();
    assert!(
        until(30, || async {
            spot_mail_queue::SentLog::is_sent(&log, &site, "default-1@example.com")
                .await
                .unwrap_or(false)
        })
        .await,
        "the default sent log is not the database one:\n{}",
        node.log()
    );
    node.stop();

    // The table exists now; the schema tool must not refuse the site over it.
    ok(&env.root.0, &["migrate", "--site", &env.site]);
    ok(&env.root.0, &["migrate", "--site", &env.site, "--apply"]);

    // A restart, the same message id again, and a different one to prove the worker ran.
    let node = Node::spawn(&env, &services, "second").await;
    send(&db, &env, "default-1@example.com", false).await;
    send(&db, &env, "default-2@example.com", true).await;
    assert!(
        until(60, || async { stub.delivered().len() >= 2 }).await,
        "the worker did not run after the restart:\n{}",
        node.log()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        stub.delivered().len(),
        2,
        "a delivered message was sent again: {:?}",
        stub.delivered()
    );
    drop(node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_lease_holder_runs_the_worker_and_a_stale_fence_is_rejected() {
    let services_cluster = "[services]\nprofile = \"cluster\"\n[services.metadata]\nbus = \"polling\"\npoll_ms = 500\n[services.jobs]\nlease_ttl_ms = 2000\n";
    let Some(env) = Env::new(services_cluster).await else {
        return;
    };
    env.site();
    let stub = Stub::start(Mode::Accept).await;
    let services = smtp(stub.port, "memory");
    let mut first = Node::spawn(&env, &services, "a").await;
    let mut second = Node::spawn(&env, &services, "b").await;
    let db = db(&env).await;
    for n in 0..3 {
        send(&db, &env, &format!("two-{n}@example.com"), true).await;
    }
    assert!(
        until(60, || async { stub.delivered().len() >= 3 }).await,
        "A:\n{}\nB:\n{}",
        first.log(),
        second.log()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(stub.delivered().len(), 3, "a message was sent twice");

    // Exactly one of the two nodes consumes the queue.
    let marker = "consuming the job queue";
    let (a, b) = (first.log().contains(marker), second.log().contains(marker));
    assert!(
        a ^ b,
        "one holder expected; A: {a}, B: {b}\nA:\n{}\nB:\n{}",
        first.log(),
        second.log()
    );
    let lease = format!("serve:{}:jobs", env.site);
    let coordinator = spot_coordinator_pg::PgCoordinator::new(
        db.backend.pool(),
        spot_coordinator_pg::PgCoordinatorConfig::default(),
    );
    let held = coordinator
        .current(&lease)
        .await
        .unwrap()
        .expect("a holder");

    // Kill the holder without a release: the other node takes over after the lease lapses,
    // with a higher fencing token, and the old token is refused.
    if a {
        first.kill9()
    } else {
        second.kill9()
    };
    for n in 3..5 {
        send(&db, &env, &format!("two-{n}@example.com"), true).await;
    }
    assert!(
        until(60, || async { stub.delivered().len() >= 5 }).await,
        "the survivor did not take over:\nA:\n{}\nB:\n{}",
        first.log(),
        second.log()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(stub.delivered().len(), 5);
    let now = coordinator
        .current(&lease)
        .await
        .unwrap()
        .expect("a new holder");
    assert!(
        now.token > held.token,
        "{:?} then {:?}",
        held.token,
        now.token
    );
    assert!(
        coordinator.check_fence(&lease, held.token).await.is_err(),
        "a stale fencing token was accepted"
    );
    assert!(coordinator.check_fence(&lease, now.token).await.is_ok());
}
