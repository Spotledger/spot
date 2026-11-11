//! WP-PRD-01.T14: the password-change route is throttled per user through the throttle store.
//!
//! The first group drives [`PasswordThrottle`] over the real audited, bounded store with a manual
//! clock (no database). The last test drives the whole route over HTTP on real PostgreSQL
//! (skipped without SPOT_TEST_PG_URL).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::audit::{
    AuditedThrottleStore, BoundedThrottleStore, CapturingAudit, IdentityAudit, Recorder,
};
use spot_cli::password_throttle::PasswordThrottle;
use spot_cli::{Options, Running};
use spot_core::{Clock, ManualClock};
use spot_login::LoginConfig;
use spot_sessions::SessionPolicy;
use spot_sessions::throttle::{ThrottleConfig, ThrottleStore};
use std::cell::Cell;
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

const USER: &str = "0192f2a0-7a3c-7b2e-9d44-5a1f0c2d3e4f";
const OTHER: &str = "0192f2a0-7a3c-7b2e-9d44-5a1f0c2d3e50";
const LIMIT: u32 = 5;

fn policy() -> ThrottleConfig {
    ThrottleConfig {
        backoff_after: LIMIT,
        lockout_after: LIMIT,
        window: Duration::from_secs(30),
        backoff_base: Duration::from_millis(1),
        backoff_multiplier: 1,
        backoff_cap: Duration::from_millis(1),
        lockout_duration: Duration::from_secs(60),
    }
}

struct Rig {
    clock: Arc<ManualClock>,
    sink: Arc<CapturingAudit>,
    store: Arc<BoundedThrottleStore>,
    throttle: PasswordThrottle<AuditedThrottleStore<BoundedThrottleStore>>,
}

fn rig(cap: usize) -> Rig {
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ));
    let sink = Arc::new(CapturingAudit::new());
    let recorder = Arc::new(Recorder::new(
        "site",
        sink.clone() as Arc<dyn IdentityAudit>,
        clock.clone() as Arc<dyn Clock>,
    ));
    let store = Arc::new(BoundedThrottleStore::with_cap(
        clock.clone() as Arc<dyn Clock>,
        cap,
    ));
    let throttle = PasswordThrottle::new(
        AuditedThrottleStore::new(store.clone(), recorder),
        policy(),
        clock.clone() as Arc<dyn Clock>,
    )
    .unwrap();
    Rig {
        clock,
        sink,
        store,
        throttle,
    }
}

/// What the route does around the password check: admit, verify (counted), record.
async fn attempt(
    rig: &Rig,
    user: &str,
    right: bool,
    verifications: &Cell<u32>,
) -> Result<(), spot_http::api::service::ApiError> {
    rig.throttle.admit(user, "req").await?;
    verifications.set(verifications.get() + 1);
    if right {
        rig.throttle.succeeded(user).await;
        Ok(())
    } else {
        rig.throttle.failed(user, "req").await?;
        Err(spot_http::api::service::ApiError::from(
            spot_http::api::registry::Code::AuthInvalidCredentials,
        ))
    }
}

#[tokio::test]
async fn the_limit_locks_the_route_before_the_password_is_verified() {
    let rig = rig(1000);
    let verified = Cell::new(0);
    for _ in 0..LIMIT {
        let refused = attempt(&rig, USER, false, &verified).await.unwrap_err();
        assert_eq!(refused.code.as_str(), "auth.invalid_credentials");
    }
    assert_eq!(verified.get(), LIMIT);
    let started = rig.sink.of("lockout_started");
    assert_eq!(started.len(), 1, "{:?}", rig.sink.records());
    assert_eq!(started[0].user.as_deref(), Some(USER));
    assert_eq!(started[0].count, Some(u64::from(LIMIT)));
    // The next attempt, even with the right password, is refused with the sign-in's lockout
    // answer and the password is never looked at.
    let right = attempt(&rig, USER, true, &verified).await.unwrap_err();
    let wrong = attempt(&rig, USER, false, &verified).await.unwrap_err();
    assert_eq!(verified.get(), LIMIT, "a locked route does no verification");
    assert_eq!(right.code.as_str(), "api.rate_limited");
    assert_eq!(
        right.details.get("retry_after").and_then(|v| v.as_u64()),
        Some(60)
    );
    assert_eq!(right.code.as_str(), wrong.code.as_str());
    assert_eq!(right.message, wrong.message);
    assert_eq!(right.details, wrong.details);
    assert!(right.details.contains_key("retry_after"));
    assert_eq!(rig.sink.of("lockout_started").len(), 1);
}

#[tokio::test]
async fn after_the_lockout_the_right_password_works_and_resets_the_counter() {
    let rig = rig(1000);
    let verified = Cell::new(0);
    for _ in 0..LIMIT {
        let _ = attempt(&rig, USER, false, &verified).await;
    }
    rig.clock.advance(Duration::from_secs(61));
    attempt(&rig, USER, true, &verified).await.unwrap();
    let ended = rig.sink.of("lockout_ended");
    assert_eq!(ended.len(), 1, "{:?}", rig.sink.records());
    assert_eq!(ended[0].user.as_deref(), Some(USER));
    let record = rig
        .store
        .get(
            true,
            PasswordThrottle::<AuditedThrottleStore<BoundedThrottleStore>>::key_of(USER).as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(record.failures, 0, "a correct password resets the counter");
    // A fresh run of wrong passwords starts from zero again.
    for _ in 0..LIMIT - 1 {
        let _ = attempt(&rig, USER, false, &verified).await;
    }
    attempt(&rig, USER, true, &verified).await.unwrap();
}

#[tokio::test]
async fn the_counter_is_independent_of_sign_in_and_of_other_users() {
    let rig = rig(1000);
    let verified = Cell::new(0);
    let key = PasswordThrottle::<AuditedThrottleStore<BoundedThrottleStore>>::key_of(USER);
    // Locking the password route writes nothing under the sign-in key (the login name) and a
    // sign-in lockout (written under the bare name) does not lock the route.
    for _ in 0..LIMIT {
        let _ = attempt(&rig, USER, false, &verified).await;
    }
    for name in ["administrator", USER] {
        let signin = rig.store.get(true, name.as_bytes()).await.unwrap();
        assert_eq!(signin.failures, 0, "the sign-in counter is untouched");
        assert_eq!(signin.locked_until, 0);
    }
    assert!(
        rig.store
            .get(true, key.as_bytes())
            .await
            .unwrap()
            .locked_until
            > 0
    );
    // Another user is not affected.
    attempt(&rig, OTHER, true, &verified).await.unwrap();
    let other = PasswordThrottle::<AuditedThrottleStore<BoundedThrottleStore>>::key_of(OTHER);
    assert_eq!(
        rig.store
            .get(true, other.as_bytes())
            .await
            .unwrap()
            .failures,
        0
    );

    // The other way: a locked sign-in for the same user does not lock the route.
    let fresh = self::rig(1000);
    let now = fresh.clock.now_utc().unix_timestamp();
    fresh
        .store
        .put(
            true,
            USER.as_bytes(),
            spot_sessions::throttle::FailureRecord {
                failures: 10,
                window_start: now,
                locked_until: now + 600,
                last_failure: now,
            },
        )
        .await
        .unwrap();
    attempt(&fresh, USER, true, &verified).await.unwrap();
    assert_eq!(
        fresh
            .store
            .get(true, USER.as_bytes())
            .await
            .unwrap()
            .locked_until,
        now + 600,
        "a password change does not clear a sign-in lockout"
    );
}

#[tokio::test]
async fn keys_are_the_user_id_under_a_prefix_and_never_a_name_or_token() {
    let rig = rig(1000);
    let verified = Cell::new(0);
    let _ = attempt(&rig, USER, false, &verified).await;
    let key = PasswordThrottle::<AuditedThrottleStore<BoundedThrottleStore>>::key_of(USER);
    assert_eq!(key, format!("pwchange:{USER}"));
    assert_eq!(
        rig.store.get(true, key.as_bytes()).await.unwrap().failures,
        1
    );
    // Exactly the account counter and the per-user source counter exist.
    assert_eq!(rig.store.len(), 2);
}

#[tokio::test]
async fn the_key_cap_evicts_instead_of_growing_and_a_table_of_lockouts_fails_closed() {
    let rig = rig(6);
    let verified = Cell::new(0);
    for n in 0..20u32 {
        let user = format!("0192f2a0-7a3c-7b2e-9d44-5a1f0c2d{n:04}");
        let _ = attempt(&rig, &user, false, &verified).await;
        assert!(rig.store.len() <= 6, "the table grew past its cap");
    }
    // Every slot a live lockout: nothing is evicted and a new failure is refused, not dropped.
    let full = self::rig(3);
    for user in [USER, OTHER] {
        for _ in 0..LIMIT {
            let _ = attempt(&full, user, false, &verified).await;
        }
    }
    let third = "0192f2a0-7a3c-7b2e-9d44-5a1f0c2d9999";
    let refused = full.throttle.failed(third, "req").await;
    assert!(refused.is_err(), "an uncountable failure must refuse");
    for user in [USER, OTHER] {
        assert!(full.throttle.admit(user, "req").await.is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_locks_per_user_keeps_the_callers_session_and_audits_it() {
    let Some(env) = Env::new().await else { return };
    let secret = secret_of(&env.ok(&["new-site", &env.site]));
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let sink = Arc::new(CapturingAudit::new());
    let (server, addr, stop) = start(&env, options(&clock), Some(sink.clone())).await;
    let host = env.site.clone();

    let first = login(addr, &host, "administrator", &secret).await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let changed = http(
        addr,
        &host,
        Some(&token_in(&first)),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": secret, "new": PASSWORD})),
    )
    .await;
    assert_eq!(changed.status, 200, "{}", changed.raw);
    let other_device = login(addr, &host, "administrator", PASSWORD).await;
    assert_eq!(other_device.status, 200, "{}", other_device.raw);
    let other_device = token_in(&other_device);
    let stolen = login(addr, &host, "administrator", PASSWORD).await;
    let stolen = token_in(&stolen);
    let change = |token: String, current: &'static str, new: &'static str| {
        let host = host.clone();
        async move {
            http(
                addr,
                &host,
                Some(&token),
                "POST",
                "/api/v1/auth/password",
                Some(json!({"current": current, "new": new})),
            )
            .await
        }
    };

    // Five wrong current passwords: five failure records, one lockout.
    for _ in 0..5 {
        assert_eq!(change(stolen.clone(), WRONG, PASSWORD).await.status, 401);
    }
    let wrong: Vec<_> = sink
        .of("password_change_failed")
        .into_iter()
        .filter(|r| r.reason.as_deref() == Some("password_change_wrong_current"))
        .collect();
    assert_eq!(wrong.len(), 5, "{:?}", sink.records());
    assert_eq!(sink.of("lockout_started").len(), 1, "{:?}", sink.records());

    // The sixth attempt with the right password is refused with the lockout and changes nothing.
    let locked = change(stolen.clone(), PASSWORD, "another-Violet-lantern-77").await;
    let locked_wrong = change(stolen.clone(), WRONG, "another-Violet-lantern-77").await;
    assert_eq!(locked.status, 429, "{}", locked.raw);
    assert_eq!(locked.status, locked_wrong.status);
    assert_eq!(locked.body["error"]["code"], "api.rate_limited");
    assert_eq!(
        locked.body["error"]["code"],
        locked_wrong.body["error"]["code"]
    );
    // Like the sign-in's 429 (tests/e2e.rs), the wait travels as the `retry_after` detail.
    assert!(locked.body["error"]["details"]["retry_after"].is_number());
    // The password did not change, and signing in with it from another device still works.
    let signin = login(addr, &host, "administrator", PASSWORD).await;
    assert_eq!(signin.status, 200, "{}", signin.raw);

    // After the lockout the right password works, keeps this session and ends the others.
    clock.advance(Duration::from_secs(61));
    let done = change(other_device.clone(), PASSWORD, "another-Violet-lantern-77").await;
    assert_eq!(done.status, 204, "{}", done.raw);
    let alive = http(
        addr,
        &host,
        Some(&other_device),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(
        alive.status, 200,
        "the caller's own session survives: {}",
        alive.raw
    );
    let gone = http(
        addr,
        &host,
        Some(&stolen),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    assert_eq!(gone.status, 401, "another session of the user is revoked");

    let everything = serde_json::to_string(&sink.records()).unwrap();
    for needle in [
        PASSWORD,
        WRONG,
        "another-Violet-lantern-77",
        other_device.as_str(),
        stolen.as_str(),
    ] {
        assert!(
            !everything.contains(needle),
            "a credential reached the audit"
        );
    }

    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}
