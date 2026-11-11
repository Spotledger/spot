//! The simulated users against a hand-written native-API stand-in (no PostgreSQL needed):
//! each user signs in for itself, requests carry the user's own token and a request id, the
//! operations send the shapes the real API takes, and refusals are classified.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use l9_scale::clock::MonotonicClock;
use l9_scale::http::Target;
use l9_scale::metrics::Class;
use l9_scale::profile::{Op, Profile};
use l9_scale::runner::{self, Executor};
use l9_scale::session::{ApiExecutor, Secret, SimUser};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Seen {
    line: String,
    auth: Option<String>,
    request_id: Option<String>,
    if_match: Option<String>,
    body: String,
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_owned())
    })
}

fn respond(status: u16, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "X",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn answer(seen: &Seen, counter: &mut u64) -> Vec<u8> {
    let line = seen.line.as_str();
    let authed = seen
        .auth
        .as_deref()
        .is_some_and(|a| a.starts_with("Bearer tok-"));
    if line == "POST /api/v1/auth/login HTTP/1.1" {
        let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
        let user = body["user"].as_str().unwrap();
        return if body["password"] == "pw" {
            respond(200, &format!("{{\"data\":{{\"token\":\"tok-{user}\"}}}}"))
        } else {
            respond(401, "{\"error\":{\"code\":\"auth.invalid\"}}")
        };
    }
    if !authed {
        return respond(401, "{\"error\":{\"code\":\"auth.required\"}}");
    }
    match line {
        "GET /api/v1/auth/session HTTP/1.1" => {
            respond(200, "{\"data\":{\"user\":{\"id\":\"u-1\"}}}")
        }
        "POST /api/v1/resource/task/query HTTP/1.1" => {
            respond(200, "{\"data\":[{\"id\":\"t-1\"},{\"id\":\"t-2\"}]}")
        }
        "GET /api/v1/meta/task HTTP/1.1" => respond(200, "{\"data\":{}}"),
        "GET /api/v1/resource/task/t-1 HTTP/1.1" | "GET /api/v1/resource/task/t-2 HTTP/1.1" => {
            respond(200, "{\"data\":{\"id\":\"t-1\"}}")
        }
        "POST /api/v1/resource/time_entry HTTP/1.1" => {
            *counter += 1;
            respond(
                201,
                &format!("{{\"data\":{{\"id\":\"e-{counter}\",\"version\":1}}}}"),
            )
        }
        "POST /api/v1/report/task_summary/run HTTP/1.1" => {
            respond(404, "{\"error\":{\"code\":\"report.not_found\"}}")
        }
        l if l.starts_with("POST /api/v1/resource/time_entry/e-") && l.contains("/post ") => {
            if seen.if_match.is_some() {
                respond(200, "{\"data\":{\"_state\":\"posted\"}}")
            } else {
                respond(409, "{}")
            }
        }
        l if l.starts_with("PATCH /api/v1/resource/time_entry/e-") => {
            if seen.if_match.as_deref() == Some("\"0\"") {
                respond(409, "{\"error\":{\"code\":\"document.conflict\"}}")
            } else {
                respond(200, "{\"data\":{\"version\":2}}")
            }
        }
        _ => respond(404, "{\"error\":{\"code\":\"route.not_found\"}}"),
    }
}

async fn fake() -> (SocketAddr, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Log = Arc::default();
    let shared = log.clone();
    let counter = Arc::new(Mutex::new(0u64));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (log, counter) = (shared.clone(), counter.clone());
            tokio::spawn(async move {
                let mut buffer: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let head_end = loop {
                        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at;
                        }
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                    let length: usize = header(&head, "content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    while buffer.len() < head_end + 4 + length {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                    let body =
                        String::from_utf8_lossy(&buffer[head_end + 4..head_end + 4 + length])
                            .into_owned();
                    buffer.drain(..head_end + 4 + length);
                    let seen = Seen {
                        line: head.lines().next().unwrap_or("").to_owned(),
                        auth: header(&head, "authorization"),
                        request_id: header(&head, "x-request-id"),
                        if_match: header(&head, "if-match"),
                        body,
                    };
                    let reply = {
                        let mut n = counter.lock().unwrap();
                        answer(&seen, &mut n)
                    };
                    log.lock().unwrap().push(seen);
                    if socket.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, log)
}

fn users(n: usize, password: &str) -> Vec<SimUser> {
    (0..n)
        .map(|i| SimUser {
            email: format!("u{i}"),
            password: Secret::new(password),
        })
        .collect()
}

async fn start(addr: SocketAddr, n: usize, password: &str) -> Result<ApiExecutor, String> {
    ApiExecutor::start(
        Target {
            addr,
            host: "demo".into(),
        },
        users(n, password),
        "run1",
        7,
        Duration::from_secs(5),
        "task_summary",
    )
    .await
}

#[tokio::test]
async fn every_operation_has_the_expected_shape_and_classification() {
    let (addr, log) = fake().await;
    let exec = start(addr, 3, "pw").await.unwrap();
    let expected = [
        (Op::SignIn, Class::Ok),
        (Op::OpenForm, Class::Ok),
        (Op::List, Class::Ok),
        (Op::CreateSubmit, Class::Ok),
        (Op::Update, Class::Ok),
        (Op::StaleUpdate, Class::ExpectedClient),
        (Op::ReportRun, Class::ExpectedClient),
    ];
    for session in 0..3u32 {
        for (op, class) in expected {
            let outcome = exec.execute(session, op).await;
            assert_eq!(outcome.class, class, "session {session} {op}: {outcome:?}");
        }
    }
    let seen = log.lock().unwrap().clone();
    // One sign-in per session for the setup, one more per SignIn operation.
    let logins = seen
        .iter()
        .filter(|s| s.line.starts_with("POST /api/v1/auth/login"))
        .count();
    assert_eq!(logins, 3 + 3);
    // Every request after a login carries that user's own token and a request id.
    for s in seen.iter().filter(|s| !s.line.contains("/auth/login")) {
        assert!(
            s.auth
                .as_deref()
                .is_some_and(|a| a.starts_with("Bearer tok-u")),
            "{s:?}"
        );
        assert!(
            s.request_id
                .as_deref()
                .is_some_and(|r| r.starts_with("l9-run1-")),
            "{s:?}"
        );
    }
    let tokens: std::collections::BTreeSet<_> =
        seen.iter().filter_map(|s| s.auth.clone()).collect();
    assert_eq!(tokens.len(), 3, "{tokens:?}");
    // The posting carries the version it was created at; the stale update carries version 0.
    assert!(
        seen.iter()
            .any(|s| s.line.contains("/post ") && s.if_match.as_deref() == Some("\"1\""))
    );
    assert!(
        seen.iter()
            .any(|s| s.line.starts_with("PATCH") && s.if_match.as_deref() == Some("\"0\""))
    );
    let create = seen
        .iter()
        .find(|s| s.line == "POST /api/v1/resource/time_entry HTTP/1.1")
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&create.body).unwrap();
    assert_eq!(body["person"], "u-1");
    assert_eq!(
        body["task"].as_str().map(|t| t.starts_with("t-")),
        Some(true)
    );
}

#[tokio::test]
async fn a_user_who_cannot_sign_in_stops_the_run_before_it_starts_and_the_password_is_not_in_the_message()
 {
    let (addr, _) = fake().await;
    let error = match start(addr, 2, "wrong-password-xyz").await {
        Ok(_) => panic!("the setup should fail"),
        Err(error) => error,
    };
    assert!(error.contains("sign-in"), "{error}");
    assert!(!error.contains("wrong-password-xyz"), "{error}");
}

#[tokio::test]
async fn a_whole_profile_runs_through_the_runner_without_failures() {
    let (addr, _) = fake().await;
    let exec = Arc::new(start(addr, 4, "pw").await.unwrap());
    let profile = Profile::from_toml(
        "name = \"fake\"\ntier = \"x\"\nsessions = 4\nramp_seconds = 1\nsteady_seconds = 3\ntarget_tps = 40\n\
         [think]\nkind = \"fixed\"\nms = 20\n\
         [mix]\nsign_in = 1\nopen_form = 4\nlist = 3\ncreate_submit = 2\nupdate = 2\nreport_run = 1\nstale_update = 1\n",
    )
    .unwrap();
    let result = runner::run(&profile, exec, Arc::new(MonotonicClock::new()), None).await;
    let all = result.metrics.all();
    assert!(all.total() > 60, "{}", all.total());
    assert_eq!(all.failures(), 0, "{all:?}");
    assert!(all.expected_client > 0);
    for op in l9_scale::profile::OPS {
        assert!(result.metrics.op(op).total() > 0, "{op} never ran");
    }
}
