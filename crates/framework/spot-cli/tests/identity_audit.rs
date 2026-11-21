//! Recorded security finding: failed sign-ins, lockouts, refusals and password changes are audited through the
//! identity audit seam, with no credential in any record, a fail-closed success path and
//! bounded growth. Real PostgreSQL, real HTTP (skipped without SPOT_TEST_PG_URL).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::audit::{CapturingAudit, IdentityAudit};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use spot_sessions::SessionPolicy;
use spot_sessions::throttle::ThrottleConfig;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const WRONG: &str = "definitely-not-the-password-1";

struct Reply {
    status: u16,
    body: Json,
    raw: String,
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
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        body: serde_json::from_str(payload).unwrap_or(Json::Null),
        raw,
    }
}

async fn login(addr: SocketAddr, host: &str, user: &str, password: &str) -> Reply {
    http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": user, "password": password})),
    )
    .await
}

fn token_in(reply: &Reply) -> String {
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

async fn start(
    env: &Env,
    options: Options,
    sink: Option<Arc<CapturingAudit>>,
) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let sink = sink.map(|s| s as Arc<dyn IdentityAudit>);
    let running = spot_cli::start_with_audit(&env.root, false, options, sink, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

fn options(clock: &Arc<spot_core::ManualClock>) -> Options {
    Options {
        clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
        login: LoginConfig {
            session_policy: SessionPolicy::DEFAULT,
            throttle: ThrottleConfig {
                backoff_after: 5,
                lockout_after: 5,
                window: Duration::from_secs(30),
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

fn secret_of(output: &str) -> String {
    output.trim().rsplit(' ').next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_lockouts_refusals_and_changes_are_audited_without_a_credential() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
    let sink = Arc::new(CapturingAudit::new());
    let (server, addr, stop) = start(&env, options(&clock), Some(sink.clone())).await;
    let host = env.site.clone();

    // First sign-in with the one-time secret, then choose a password.
    let first = login(addr, &host, "administrator", &secret).await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let restricted = token_in(&first);
    // A wrong current password is audited as a failed change.
    let bad = http(
        addr,
        &host,
        Some(&restricted),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": WRONG, "new": PASSWORD})),
    )
    .await;
    assert_eq!(bad.status, 401, "{}", bad.raw);
    assert_eq!(sink.of("password_change_failed").len(), 1);
    let changed = http(
        addr,
        &host,
        Some(&restricted),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": PASSWORD})),
    )
    .await;
    assert_eq!(changed.status, 200, "{}", changed.raw);
    let full = token_in(&changed);
    assert_eq!(sink.of("password_changed").len(), 1);
    assert!(
        !sink.of("session_revoked").is_empty(),
        "the restricted session's end is audited"
    );
    let signed_in_before = sink.of("sign_in_succeeded").len();
    assert!(signed_in_before >= 1);

    // Five wrong passwords: exactly five failure records and one lockout record.
    let baseline = login(addr, &host, "administrator", WRONG).await;
    assert_eq!(baseline.status, 401);
    for _ in 0..4 {
        assert_eq!(login(addr, &host, "administrator", WRONG).await.status, 401);
    }
    assert_eq!(sink.of("sign_in_failed").len(), 5, "{:?}", sink.records());
    let started = sink.of("lockout_started");
    assert_eq!(started.len(), 1, "{:?}", sink.records());
    assert_eq!(started[0].attempted.as_deref(), Some("administrator"));
    assert_eq!(started[0].count, Some(5));

    // The right password during the lockout is refused, and the refusal is audited.
    let during = login(addr, &host, "administrator", PASSWORD).await;
    assert_ne!(during.status, 200, "{}", during.raw);
    assert_eq!(sink.of("sign_in_succeeded").len(), signed_in_before);
    let refused = sink.of("sign_in_refused");
    assert_eq!(refused.len(), 1, "{:?}", sink.records());
    assert_eq!(refused[0].reason.as_deref(), Some("account_locked"));

    // After the lockout ends the right password works again; both are audited.
    clock.advance(Duration::from_secs(61));
    let after = login(addr, &host, "administrator", PASSWORD).await;
    assert_eq!(after.status, 200, "{}", after.raw);
    let after_token = token_in(&after);
    assert_eq!(sink.of("sign_in_succeeded").len(), signed_in_before + 1);
    let ended = sink.of("lockout_ended");
    assert_eq!(ended.len(), 1, "{:?}", sink.records());
    assert_eq!(ended[0].reason.as_deref(), Some("expired"));

    // An unknown user is indistinguishable to the caller, but audited with the redacted name.
    let unknown = login(addr, &host, "nobody-by-this-name", WRONG).await;
    let known = login(addr, &host, "administrator", WRONG).await;
    assert_eq!(unknown.status, known.status);
    assert_eq!(unknown.body["error"]["code"], known.body["error"]["code"]);
    assert_eq!(
        unknown.body["error"]["message"],
        known.body["error"]["message"]
    );
    assert!(
        sink.records()
            .iter()
            .any(|r| r.attempted.as_deref() == Some("nobody-by-this-name"))
    );
    // A long name with a newline is truncated and flattened; a pasted key is dropped.
    let long = format!("{}\nINJECTED", "a b ".repeat(100));
    assert_eq!(login(addr, &host, &long, WRONG).await.status, 401);
    assert_eq!(login(addr, &host, &secret, WRONG).await.status, 401);
    let names: Vec<String> = sink
        .records()
        .iter()
        .filter_map(|r| r.attempted.clone())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| n.chars().count() <= 128 && !n.contains('\n'))
    );
    assert!(names.iter().any(|n| n == "[redacted]"));

    // Sink trouble on a failure path changes nothing the caller sees.
    sink.fail_all(true);
    let blind = login(addr, &host, "administrator", WRONG).await;
    assert_eq!(blind.status, known.status);
    assert_eq!(blind.body["error"]["code"], known.body["error"]["code"]);
    assert_eq!(
        blind.body["error"]["message"],
        known.body["error"]["message"]
    );
    // A success whose audit cannot be written is refused (fail closed).
    clock.advance(Duration::from_secs(700));
    let refused_success = login(addr, &host, "administrator", PASSWORD).await;
    assert_ne!(refused_success.status, 200, "{}", refused_success.raw);
    assert!(refused_success.body["data"]["token"].is_null());
    sink.fail_all(false);

    // Nothing in any record holds a credential.
    let everything = serde_json::to_string(&sink.records()).unwrap();
    for needle in [
        secret.as_str(),
        restricted.as_str(),
        full.as_str(),
        after_token.as_str(),
        PASSWORD,
        WRONG,
    ] {
        assert!(
            !everything.contains(needle),
            "a credential reached the audit"
        );
    }
    let key_id = secret.split('_').nth(1).unwrap();
    assert!(!everything.contains(key_id));

    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_mirror_writes_a_private_audit_file_under_the_site() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    // The chain is the production sink (tests/identity_chain.rs); the file is its optional
    // mirror, switched on here.
    let common = env.root.join("common.toml");
    let text = std::fs::read_to_string(&common).unwrap();
    std::fs::write(
        &common,
        format!("{text}\n[audit]\nidentity_file_mirror = true\n"),
    )
    .unwrap();
    let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
    let (server, addr, stop) = start(&env, options(&clock), None).await;
    assert_eq!(
        login(addr, &env.site, "administrator", WRONG).await.status,
        401
    );
    assert_eq!(
        login(addr, &env.site, "administrator", &secret)
            .await
            .status,
        200
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
    let path = env
        .root
        .join("sites")
        .join(&env.site)
        .join("identity-audit.log");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"kind\":\"sign_in_failed\""), "{text}");
    assert!(text.contains("\"kind\":\"sign_in_succeeded\""), "{text}");
    assert!(!text.contains(&secret) && !text.contains(WRONG));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

/// The session read refuses every API key, so a valid key is shown to authenticate by
/// the answer of an authenticated route instead: the new site's key is still restricted until a
/// password is chosen, so the gate answers it `403 cli.password_change_required`, which only an
/// authenticated principal gets (a refused credential is `401`).
fn assert_authenticated(reply: &Reply) {
    assert_eq!(reply.status, 403, "{}", reply.raw);
    assert_eq!(reply.body["error"]["code"], "cli.password_change_required");
}

/// WP-PRD-01.T15: a refused API key is audited without key material, answered exactly as before
/// whether the audit works or not, and a wrong secret for a real id reads like an unknown id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_api_key_is_audited_without_key_material() {
    let Some(env) = Env::new().await else { return };
    let key_file = env.root.join("admin.key");
    env.ok(&[
        "new-site",
        &env.site,
        "--admin-api-key-file",
        key_file.to_str().unwrap(),
    ]);
    let key = std::fs::read_to_string(&key_file)
        .unwrap()
        .trim()
        .to_owned();
    let (id, secret) = key
        .strip_prefix("spk_")
        .unwrap()
        .split_once('_')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap();
    let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
    let sink = Arc::new(CapturingAudit::new());
    let (server, addr, stop) = start(&env, options(&clock), Some(sink.clone())).await;
    let host = env.site.clone();
    let probe = |token: Option<String>| {
        let host = host.clone();
        async move {
            http(
                addr,
                &host,
                token.as_deref(),
                "GET",
                "/api/v1/meta/task",
                None,
            )
            .await
        }
    };

    // A valid key and no credential at all leave no rejection record.
    assert_authenticated(&probe(Some(key.clone())).await);
    assert_eq!(probe(None).await.status, 401);
    assert!(
        sink.of("api_key_rejected").is_empty(),
        "{:?}",
        sink.records()
    );

    // Same length as a real secret, so the shape is right and only the secret is wrong.
    let wrong = format!("spk_{id}_{}", "A".repeat(secret.len()));
    let fake_id = format!("spk_{}_{secret}", "a".repeat(id.len()));
    let wrong_reply = probe(Some(wrong.clone())).await;
    let unknown_reply = probe(Some(fake_id.clone())).await;
    let malformed_reply = probe(Some("spk_not-a-key".into())).await;
    for reply in [&wrong_reply, &unknown_reply, &malformed_reply] {
        assert_eq!(reply.status, 401, "{}", reply.raw);
        assert_eq!(
            reply.body["error"]["code"],
            wrong_reply.body["error"]["code"]
        );
    }
    let rejected = sink.of("api_key_rejected");
    assert_eq!(rejected.len(), 3, "{:?}", sink.records());
    for record in &rejected {
        assert_eq!(record.path.as_deref(), Some("api"));
        assert!(record.source.is_none() && record.user.is_none() && record.attempted.is_none());
    }
    assert_eq!(rejected[0].reason.as_deref(), Some("unknown"));
    assert_eq!(rejected[0].key_id.as_deref(), Some(id.as_str()));
    assert_eq!(rejected[1].reason.as_deref(), Some("unknown"));
    assert_eq!(
        rejected[1].key_id.as_deref(),
        Some(fake_id[4..16].to_owned().as_str())
    );
    assert_eq!(rejected[2].reason.as_deref(), Some("malformed"));
    assert_eq!(rejected[2].key_id, None);
    // No secret, no full credential and no URL in any serialised record.
    let bytes = serde_json::to_string(&sink.records()).unwrap();
    for needle in [
        secret.as_str(),
        key.as_str(),
        wrong.as_str(),
        "/api/v1",
        "not-a-key",
    ] {
        assert!(!bytes.contains(needle), "{needle} leaked: {bytes}");
    }

    // A rejection counts toward no lockout: the key still works afterwards.
    assert_authenticated(&probe(Some(key.clone())).await);

    // An expired key (right secret) is `expired`; the caller still gets the same refusal.
    clock.advance(Duration::from_secs(60 * 60 * 24 * 400));
    let expired = probe(Some(key.clone())).await;
    assert_eq!(expired.status, 401, "{}", expired.raw);
    let last = sink.of("api_key_rejected").pop().unwrap();
    assert_eq!(last.reason.as_deref(), Some("expired"));
    assert_eq!(last.key_id.as_deref(), Some(id.as_str()));

    // An audit outage never changes the answer.
    sink.fail_all(true);
    let down = probe(Some(wrong)).await;
    assert_eq!(down.status, wrong_reply.status);
    assert_eq!(
        down.body["error"]["code"],
        wrong_reply.body["error"]["code"]
    );
    assert_eq!(
        down.body["error"]["message"],
        wrong_reply.body["error"]["message"]
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}
