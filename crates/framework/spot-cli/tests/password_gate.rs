//! Recorded security findings: a one-time first sign-in secret can only choose a password, it expires,
//! it is not a bearer key, and nothing secret is logged. Real PostgreSQL, real HTTP.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use support::{Env, text};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

// ---- a capturing tracing subscriber (no extra dependency) --------------------------------

static CAPTURED: OnceLock<Arc<Mutex<String>>> = OnceLock::new();

struct Capture {
    out: Arc<Mutex<String>>,
    next: AtomicU64,
}

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = format!("span {}", attributes.metadata().name());
        attributes.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
        tracing::span::Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::from("record");
        values.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = format!("event {}", event.metadata().target());
        event.record(&mut Fields(&mut line));
        self.out.lock().unwrap().push_str(&format!("{line}\n"));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Installs the capturing subscriber once for this test binary and returns its buffer.
fn capture() -> Arc<Mutex<String>> {
    CAPTURED
        .get_or_init(|| {
            let out = Arc::new(Mutex::new(String::new()));
            let _ = tracing::subscriber::set_global_default(Capture {
                out: out.clone(),
                next: AtomicU64::new(0),
            });
            out
        })
        .clone()
}

// ---- helpers -----------------------------------------------------------------------------

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

fn options(clock: Option<Arc<spot_core::ManualClock>>) -> Options {
    Options {
        clock: clock.map(|c| c as Arc<dyn spot_core::Clock>),
        login: LoginConfig::for_tests(),
    }
}

async fn start(
    env: &Env,
    options: Options,
) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(&env.root, false, options, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

fn secret_of(output: &str) -> String {
    output.trim().rsplit(' ').next().unwrap().to_owned()
}

/// Every route and method the restricted session must not reach. Existing and non-existing
/// paths alike: the gate answers before the router can say 404 or 405.
const REFUSED: &[(&str, &str)] = &[
    ("GET", "/api/v1/meta/task"),
    ("GET", "/api/v1/resource/user"),
    ("GET", "/api/v1/resource/user?limit=1"),
    ("POST", "/api/v1/resource/project"),
    (
        "PATCH",
        "/api/v1/resource/project/00000000-0000-7000-8000-000000000000",
    ),
    (
        "DELETE",
        "/api/v1/resource/project/00000000-0000-7000-8000-000000000000",
    ),
    ("GET", "/api/v1/auth/permissions"),
    ("GET", "/api/v1/auth/password"),
    ("PUT", "/api/v1/auth/password"),
    ("POST", "/api/v1/auth/session"),
    ("GET", "/api/v1/auth/logout"),
    ("GET", "/api/v1/auth/session/"),
    ("HEAD", "/api/v1/auth/session"),
    ("GET", "/api/v1/no-such-route"),
    // Reviewer additions: methods and spellings of the allowed paths that are not the exact
    // allowed ones (case, encoding, extra slashes, dot segments, other methods).
    ("OPTIONS", "/api/v1/auth/session"),
    ("DELETE", "/api/v1/auth/session"),
    ("OPTIONS", "/api/v1/meta/task"),
    ("GET", "//api/v1/auth/session"),
    ("GET", "/api/v1/auth/%73ession"),
    ("GET", "/api/v1/auth/session%2F"),
    ("GET", "/API/V1/AUTH/SESSION"),
    ("GET", "/api/v1/auth/./session"),
    ("GET", "/api/v1/auth/x/../session"),
    ("POST", "/api/v1/auth/password/"),
    ("POST", "/api/v1/auth/%70assword"),
    ("POST", "/api/v1/auth/password%00"),
    ("GET", "/metrics"),
];

async fn assert_gated(addr: SocketAddr, host: &str, token: &str) {
    for (method, path) in REFUSED {
        let reply = http(addr, host, Some(token), method, path, None).await;
        // `/metrics` is only mounted when configured; anything else is the gate's answer.
        if *path == "/metrics" && reply.status == 404 {
            continue;
        }
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.raw);
        if *method != "HEAD" {
            assert_eq!(
                reply.body["error"]["code"], "cli.password_change_required",
                "{method} {path}: {}",
                reply.raw
            );
            assert!(reply.body.get("data").is_none(), "{method} {path}");
        }
    }
}

// ---- the tests ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sign_in_session_can_only_change_the_password() {
    let log = capture();
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let (server, addr, stop) = start(&env, options(None)).await;
    let host = env.site.clone();

    let first = login(addr, &host, "administrator", &secret).await;
    assert_eq!(first.status, 200, "{}", first.raw);
    assert_eq!(
        first.body["messages"][0]["code"],
        "auth.password_change_required"
    );
    let restricted = token_in(&first);
    assert_gated(addr, &host, &restricted).await;

    // The three allowed operations: reading the session (the display name) works.
    let session = http(
        addr,
        &host,
        Some(&restricted),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(session.status, 200, "{}", session.raw);
    assert!(session.body["data"]["user"]["full_name"].is_string());
    // A wrong current password changes nothing, and the session stays restricted.
    let wrong = http(
        addr,
        &host,
        Some(&restricted),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": "not-the-secret", "new": PASSWORD})),
    )
    .await;
    assert_eq!(wrong.status, 401, "{}", wrong.raw);
    assert_gated(addr, &host, &restricted).await;

    // The change replaces the session: a new id, the old one revoked.
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
    assert_ne!(full, restricted);
    let old = http(
        addr,
        &host,
        Some(&restricted),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(old.status, 401, "the old session is revoked: {}", old.raw);
    let meta = http(addr, &host, Some(&full), "GET", "/api/v1/meta/task", None).await;
    assert_eq!(meta.status, 200, "{}", meta.raw);
    let users = http(
        addr,
        &host,
        Some(&full),
        "GET",
        "/api/v1/resource/user",
        None,
    )
    .await;
    assert_eq!(users.status, 200, "{}", users.raw);

    // The secret is spent: not as a password, not as a bearer credential.
    assert_eq!(
        login(addr, &host, "administrator", &secret).await.status,
        401
    );
    let bearer = http(addr, &host, Some(&secret), "GET", "/api/v1/meta/task", None).await;
    assert_eq!(bearer.status, 401, "{}", bearer.raw);

    stop.send(()).unwrap();
    server.handle.join().await.unwrap();

    // Nothing the server or the commands wrote carries a secret, a token or the password.
    tracing::info!("canary");
    let logged = log.lock().unwrap().clone();
    assert!(logged.contains("canary"), "the capture works");
    for needle in [
        secret.as_str(),
        restricted.as_str(),
        full.as_str(),
        PASSWORD,
    ] {
        assert!(!logged.contains(needle), "a secret was logged");
    }
    let key_id = secret.split('_').nth(1).unwrap();
    assert!(!logged.contains(key_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_one_time_secret_is_not_a_bearer_key_and_sign_out_works_while_restricted() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let site = env.site_file();
    assert!(!site.contains("[[api_keys]]"), "no key verifier: {site}");
    assert!(site.contains("first_sign_in = true") && site.contains("expires_at = "));
    let (server, addr, stop) = start(&env, options(None)).await;
    let host = env.site.clone();

    // As a bearer credential the secret is refused, whatever the route.
    for path in ["/api/v1/meta/task", "/api/v1/auth/session"] {
        let reply = http(addr, &host, Some(&secret), "GET", path, None).await;
        assert_eq!(reply.status, 401, "{path}: {}", reply.raw);
    }
    let restricted = token_in(&login(addr, &host, "administrator", &secret).await);
    let out = http(
        addr,
        &host,
        Some(&restricted),
        "POST",
        "/api/v1/auth/logout",
        None,
    )
    .await;
    assert_eq!(out.status, 204, "{}", out.raw);
    let after = http(
        addr,
        &host,
        Some(&restricted),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(after.status, 401);
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_secret_is_a_wrong_password_and_reset_admin_issues_a_new_one() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site, "--secret-hours", "10"]));
    let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
    let (server, addr, stop) = start(&env, options(Some(clock.clone()))).await;
    let host = env.site.clone();

    let wrong = login(addr, &host, "administrator", "not-the-secret-at-all").await;
    assert_eq!(wrong.status, 401);
    // Just before it expires the secret still works (and is restricted).
    clock.advance(Duration::from_secs(9 * 3600));
    let early = login(addr, &host, "administrator", &secret).await;
    assert_eq!(early.status, 200, "{}", early.raw);
    let early_token = token_in(&early);
    // After the expiry: the same status and body as a wrong password.
    clock.advance(Duration::from_secs(2 * 3600));
    let expired = login(addr, &host, "administrator", &secret).await;
    assert_eq!(expired.status, wrong.status);
    assert_eq!(
        expired.body["error"]["code"], wrong.body["error"]["code"],
        "{}",
        expired.raw
    );
    assert_eq!(
        expired.body["error"]["message"],
        wrong.body["error"]["message"]
    );
    assert_eq!(
        expired.body["error"]["details"],
        wrong.body["error"]["details"]
    );
    // A session that outlives the secret still cannot choose a password with it.
    let late = http(
        addr,
        &host,
        Some(&early_token),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": PASSWORD})),
    )
    .await;
    assert!(late.status == 401 || late.status == 403, "{}", late.raw);
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();

    // reset-admin issues a new secret; the old one stays dead.
    let renewed = secret_of(&env.ok(&["reset-admin", &env.site, "--yes-i-am-the-operator"]));
    assert_ne!(renewed, secret);
    let (server, addr, stop) = start(&env, options(None)).await;
    assert_eq!(
        login(addr, &host, "administrator", &secret).await.status,
        401
    );
    assert_eq!(
        login(addr, &host, "administrator", &renewed).await.status,
        200
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lifetime_options_are_capped() {
    let Some(env) = Env::new().await else { return };
    for args in [
        vec!["new-site", &env.site, "--secret-hours", "337"],
        vec!["new-site", &env.site, "--secret-hours", "0"],
        vec![
            "new-site",
            &env.site,
            "--admin-api-key-file",
            "/tmp/never-written-spot-key",
            "--admin-api-key-hours",
            "169",
        ],
    ] {
        let output = env.spot(&args);
        assert!(!output.status.success(), "{args:?}: {}", text(&output));
    }
    assert!(!std::path::Path::new("/tmp/never-written-spot-key").exists());
    assert!(
        !env.root.join("sites").join(&env.site).exists(),
        "a refused option creates nothing"
    );
    // The maximum is accepted.
    env.ok(&["new-site", &env.site, "--secret-hours", "336"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bootstrap_api_key_is_a_separate_file_with_an_expiry_and_the_same_gate() {
    let Some(env) = Env::new().await else { return };
    let key_file = env.root.join("admin.key");
    let out = env.ok(&[
        "new-site",
        &env.site,
        "--admin-api-key-file",
        key_file.to_str().unwrap(),
    ]);
    let secret = secret_of(&out);
    let key = std::fs::read_to_string(&key_file)
        .unwrap()
        .trim()
        .to_owned();
    assert!(key.starts_with("spk_") && key != secret);
    assert!(!out.contains(&key), "the key is never printed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let site = env.site_file();
    assert!(!site.contains(&key) && site.matches("[[api_keys]]").count() == 1);
    assert!(site.matches("expires_at = ").count() == 2, "{site}");

    // An existing path is refused and nothing is created.
    let again = env.spot(&[
        "new-site",
        &format!("{}x", env.site),
        "--admin-api-key-file",
        key_file.to_str().unwrap(),
    ]);
    assert!(!again.status.success());
    assert!(
        !env.root
            .join("sites")
            .join(format!("{}x", env.site))
            .exists()
    );
    assert_eq!(std::fs::read_to_string(&key_file).unwrap().trim(), key);

    let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
    let (server, addr, stop) = start(&env, options(Some(clock.clone()))).await;
    let host = env.site.clone();
    // Until the password is chosen the key is gated like the session.
    let denied = http(addr, &host, Some(&key), "GET", "/api/v1/meta/task", None).await;
    assert_eq!(denied.status, 403, "{}", denied.raw);
    assert_eq!(denied.body["error"]["code"], "cli.password_change_required");
    // The gate lets the session read through to its handler, which refuses every API key:
    // `permission.denied`, not the gate's own refusal.
    let own = http(addr, &host, Some(&key), "GET", "/api/v1/auth/session", None).await;
    assert_eq!(own.status, 403, "{}", own.raw);
    assert_eq!(own.body["error"]["code"], "permission.denied");
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();

    // `spot set-password` lifts the gate for the key, and the key still expires.
    let set = support::run(
        &env.root,
        &[
            "set-password",
            &env.site,
            "administrator",
            "--password-stdin",
        ],
        Some(&format!("{PASSWORD}\n")),
    );
    assert!(set.status.success(), "{}", text(&set));
    let (server, addr, stop) = start(&env, options(Some(clock.clone()))).await;
    let allowed = http(addr, &host, Some(&key), "GET", "/api/v1/meta/task", None).await;
    assert_eq!(allowed.status, 200, "{}", allowed.raw);
    clock.advance(Duration::from_secs(25 * 3600));
    let expired = http(addr, &host, Some(&key), "GET", "/api/v1/meta/task", None).await;
    assert_eq!(expired.status, 401, "{}", expired.raw);
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_from_an_earlier_version_is_treated_as_expired_with_a_message() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    // Rewrite the site as an earlier version wrote it: no expiry on the verifier.
    let path = env.root.join("sites").join(&env.site).join("site.toml");
    let site = std::fs::read_to_string(&path).unwrap();
    let legacy: String = site
        .lines()
        .filter(|l| !l.starts_with("expires_at"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&path, legacy).unwrap();

    let check = env.ok(&["check"]);
    assert!(
        check.contains("notice:") && check.contains("treated as expired"),
        "{check}"
    );
    assert!(!check.contains(&secret));
    let (server, addr, stop) = start(&env, options(None)).await;
    let host = env.site.clone();
    assert_eq!(
        login(addr, &host, "administrator", &secret).await.status,
        401
    );
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
    // The file was not extended behind the operator's back.
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("expires_at")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invited_user_gets_the_same_restricted_expiring_secret() {
    let Some(env) = Env::new().await else { return };
    env.ok(&["new-site", &env.site]);
    let out = env.ok(&[
        "add-user",
        &env.site,
        "ama@example.com",
        "--secret-hours",
        "5",
    ]);
    let secret = secret_of(&out);
    assert!(out.contains("good for 5 hours"), "{out}");
    let (server, addr, stop) = start(&env, options(None)).await;
    let host = env.site.clone();
    let reply = login(addr, &host, "ama@example.com", &secret).await;
    assert_eq!(reply.status, 200, "{}", reply.raw);
    let token = token_in(&reply);
    assert_gated(addr, &host, &token).await;
    let bearer = http(
        addr,
        &host,
        Some(&secret),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(bearer.status, 401);
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

/// The secret part of `spk_<key_id>_<secret>`. The secret part is 43 characters of
/// `[A-Za-z0-9_-]`, so it can itself contain `_`: split into three, never on the last `_`
/// (which can leave a one-character "body" that occurs in any fixed or random text).
fn secret_part(text: &str) -> &str {
    let mut parts = text.splitn(3, '_');
    assert_eq!(parts.next(), Some("spk"));
    let _key_id = parts.next().unwrap();
    let secret = parts.next().unwrap();
    assert_eq!(secret.len(), spot_auth::api_key::API_KEY_SECRET_LENGTH);
    secret
}

#[test]
fn secrets_do_not_appear_in_debug_output() {
    use spot_auth::{HashParams, Password, PasswordHash, verify_password};
    let (secret, credential) =
        spot_cli::users::issue_first_sign_in(spot_storage_contract::RecordId::new_v7(), 1_000);
    let text = secret.text();
    let body = secret_part(&text);
    // The Debug output is fixed apart from the public key id: the secret part is redacted.
    let debug = format!("{secret:?}");
    assert!(debug.contains("***"), "{debug}");
    assert!(!debug.contains(body), "{debug}");
    assert!(!debug.contains(&text), "{debug}");
    // The stored form is an Argon2id verifier, not the secret: it carries the scheme marker,
    // never the full secret text or its 43-character secret part, it accepts the secret and
    // rejects any other.
    let stored = &credential.password_hash;
    assert!(stored.starts_with("$argon2id$"), "{stored}");
    assert!(!stored.contains(&text) && !stored.contains(body));
    let params = HashParams::default();
    let parsed = PasswordHash::parse(stored).unwrap();
    assert!(verify_password(&Password::new(&text), &parsed, &params).is_ok());
    let (other, _) = spot_auth::ApiKey::generate();
    assert!(verify_password(&Password::new(&other.text()), &parsed, &params).is_err());
    assert!(credential.first_sign_in && credential.expires_at == Some(1_000));
}

/// Over 2,000 fresh secrets and salts the stored form never contains the secret and always
/// verifies (the old check, a short fragment against random output, failed about 1 run in 70).
/// Cheap test parameters keep the loop fast; the hash format is the same.
#[test]
fn a_stored_verifier_never_contains_its_secret_over_many_fresh_secrets() {
    use spot_auth::{ApiKey, HashParams, Password, hash_password, verify_password};
    let params = HashParams::for_tests();
    for _ in 0..2_000 {
        let (secret, _) = ApiKey::generate();
        let text = secret.text();
        let body = secret_part(&text);
        let hash = hash_password(&Password::new(&text), &params);
        let stored = hash.phc_string();
        assert!(stored.starts_with("$argon2id$"), "{stored}");
        assert!(
            !stored.contains(&text) && !stored.contains(body),
            "{stored}"
        );
        assert!(verify_password(&Password::new(&text), &hash, &params).is_ok());
        let debug = format!("{secret:?}");
        assert!(debug.contains("***") && !debug.contains(body), "{debug}");
    }
}
