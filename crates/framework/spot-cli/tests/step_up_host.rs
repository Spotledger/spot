//! WP-IAM-12.T8: the host serves step-up for real (`auth_step_up`, `POST /api/v1/auth/step-up`)
//! and the operations that need a recent step-up honour the marker it writes. Real PostgreSQL, a
//! real `spot serve`, real HTTP (skipped without `SPOT_TEST_PG_URL`).
//!
//! Security properties proved here: a fresh sign-in is **not** a step-up (the behaviour change of
//! this task); a right password makes the caller's own session recent, and no other session of the
//! same user; a wrong password writes no marker, answers as the sign-in does, and is throttled
//! before the credential is checked; a user with a second factor needs the code (once per step); the
//! window lapses with the injected clock; an API key is refused; single sign-on is not
//! configured; no password or code is in any answer or event; success and refusal are audited as
//! identity events.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use hmac::{Hmac, Mac};
use serde_json::{Value as Json, json};
use sha1::Sha1;
use spot_cli::{Options, Running};
use spot_core::Clock as _;
use spot_login::LoginConfig;
use spot_storage_contract::{ChangeFeed, FeedPosition};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use support::{Env, run};
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const WRONG: &str = "not-the-password-at-all-1234";
const BOBBY: &str = "bobby@example.com";
/// The window the site is configured with, in seconds.
const WINDOW: i64 = 60;

struct Reply {
    status: u16,
    body: Json,
    raw: String,
}

impl Reply {
    fn code(&self) -> String {
        self.body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// A response header's value (case-insensitive name).
    fn header(&self, name: &str) -> Option<String> {
        let head = self.raw.split("\r\n\r\n").next()?;
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
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
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
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

/// The RFC 6238 code (HMAC-SHA1, six digits, 30 s steps) of a base32 secret at `unix`.
fn totp_code(secret_base32: &str, unix: i64) -> String {
    let key = spot_auth::base32_decode(secret_base32).unwrap();
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).unwrap();
    mac.update(&u64::try_from(unix / 30).unwrap().to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}

/// The error body without the per-request id, so two answers can be compared.
fn shape(reply: &Reply) -> (u16, Json) {
    let mut body = reply.body.clone();
    if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
        error.remove("request_id");
    }
    (reply.status, body)
}

fn set_password(env: &Env, user: &str) {
    let output = run(
        &env.root,
        &["set-password", &env.site, user, "--password-stdin"],
        Some(&format!("{PASSWORD}\n")),
    );
    assert!(output.status.success(), "{}", support::text(&output));
}

struct Site {
    env: Env,
    server: Running,
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    clock: Arc<spot_core::ManualClock>,
    /// The Administrator's API key.
    key: String,
}

impl Site {
    async fn start() -> Option<Site> {
        Self::start_with(None).await
    }

    /// A site with the step-up window at [`WINDOW`] seconds and, when given, the per-account
    /// sign-in throttle capacity (attempts per 15 minutes; the harness default is unreachable).
    async fn start_with(account_capacity: Option<u64>) -> Option<Site> {
        Self::start_with_extra(account_capacity, "").await
    }

    /// As [`Site::start_with`], with `extra` appended to the site configuration.
    async fn start_with_extra(account_capacity: Option<u64>, extra: &str) -> Option<Site> {
        let env = Env::new().await?;
        let common = env.root.join("common.toml");
        let mut text = std::fs::read_to_string(&common).unwrap();
        if let Some(capacity) = account_capacity {
            let from = "per_account = { capacity = 100000";
            assert!(text.contains(from), "the harness throttle setting moved");
            text = text.replace(from, &format!("per_account = {{ capacity = {capacity}"));
        }
        text.push_str(&format!(
            "[services.identity_admin]\nstep_up_window_secs = {WINDOW}\n"
        ));
        text.push_str(extra);
        std::fs::write(&common, text).unwrap();
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
        env.ok(&["add-user", &env.site, BOBBY]);
        set_password(&env, "administrator");
        set_password(&env, BOBBY);
        let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let options = Options {
            clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
            login: LoginConfig::for_tests(),
        };
        let server = spot_cli::start_with(&env.root, false, options, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
        let addr = server.local_addrs()[0];
        Some(Site {
            env,
            server,
            addr,
            stop,
            clock,
            key,
        })
    }

    async fn call(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Json>,
    ) -> Reply {
        http(
            self.addr,
            &self.env.site,
            token,
            method,
            path,
            headers,
            body,
        )
        .await
    }

    /// Signs in; the clock moves one second first so the sessions are ordered.
    async fn login(&self, user: &str) -> String {
        self.clock.advance(Duration::from_secs(1));
        let reply = self
            .call(
                None,
                "POST",
                "/api/v1/auth/login",
                &[],
                Some(json!({"user": user, "password": PASSWORD})),
            )
            .await;
        assert_eq!(reply.status, 200, "{}", reply.raw);
        reply.body["data"]["token"].as_str().unwrap().to_owned()
    }

    async fn step_up(&self, token: &str, body: Json) -> Reply {
        self.call(Some(token), "POST", "/api/v1/auth/step-up", &[], Some(body))
            .await
    }

    /// An operation that needs a recent step-up (creating an API key). The step-up check comes
    /// before the body is looked at, so what matters is whether the answer is
    /// `auth.step_up_required`.
    async fn needs_step_up(&self, token: &str) -> Reply {
        self.call(
            Some(token),
            "POST",
            "/api/v1/identity/api-keys",
            &[(
                "Idempotency-Key",
                concat!("0190a1b2-c3d4-7e5f-8a6b-", "1c2d3e4f5a6b"),
            )],
            Some(json!({"name": "step-up-probe"})),
        )
        .await
    }

    /// A write of the single sign-on administration (deleting a provider that does not exist).
    /// The route asks `SsoAdminPort::step_up_current` before the provider is looked up, so what
    /// matters is whether the answer is `auth.step_up_required`.
    async fn sso_write(&self, token: &str) -> Reply {
        self.call(
            Some(token),
            "DELETE",
            "/api/v1/identity/sso/providers/step-up-probe",
            &[("If-Match", "\"1\"")],
            None,
        )
        .await
    }

    /// Every outbox event, as (topic, payload).
    async fn events(&self) -> Vec<(String, Json)> {
        use spot_cli::config;
        let loaded = config::load(&self.env.root, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
        backend
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|record| {
                (
                    record.event.topic.as_str().to_owned(),
                    serde_json::from_str(&record.event.payload).unwrap_or(Json::Null),
                )
            })
            .collect()
    }

    /// The `data` of every event of `topic` (a step-up success also says `step_up: true`).
    async fn events_of(&self, topic: &str) -> Vec<Json> {
        self.events()
            .await
            .into_iter()
            .filter(|(t, _)| t == topic)
            .map(|(_, payload)| payload["data"].clone())
            .collect()
    }

    async fn finish(self) {
        self.stop.send(()).unwrap();
        self.server.handle.join().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_sign_in_is_not_a_step_up_and_a_password_step_up_makes_the_session_recent() {
    let Some(site) = Site::start().await else {
        return;
    };
    let token = site.login("administrator").await;

    // The behaviour change: a session that was just created has no marker, so it is refused.
    let before = site.needs_step_up(&token).await;
    assert_eq!(before.status, 403, "{}", before.raw);
    assert_eq!(before.code(), "auth.step_up_required");

    let reply = site.step_up(&token, json!({ "password": PASSWORD })).await;
    assert_eq!(reply.status, 200, "{}", reply.raw);
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    assert!(reply.body["data"]["redirect_url"].is_null());
    // `step_up_until` is now plus the window, in UTC whole seconds.
    let expected = (site.clock.now_utc() + time::Duration::seconds(WINDOW))
        .replace_nanosecond(0)
        .unwrap()
        .format(&Rfc3339)
        .unwrap();
    assert_eq!(reply.body["data"]["step_up_until"], json!(expected));
    assert!(expected.ends_with('Z'));

    // The marker flipped `is_recent` from false to true: the same operation is no longer asked
    // to step up.
    let after = site.needs_step_up(&token).await;
    assert_ne!(after.code(), "auth.step_up_required", "{}", after.raw);

    // The success is audited as a sign-in event with `step_up: true`: who, which session, the
    // method. No credential is in the answer or in any event.
    let succeeded = site.events_of("identity_sign_in_succeeded").await;
    let stepped: Vec<&Json> = succeeded
        .iter()
        .filter(|d| d["step_up"] == json!(true))
        .collect();
    assert_eq!(stepped.len(), 1, "{succeeded:?}");
    assert_eq!(stepped[0]["method"], "password");
    assert!(stepped[0]["user"].is_string() && stepped[0]["session"].is_string());
    assert!(!reply.raw.contains(PASSWORD));
    for (topic, payload) in site.events().await {
        assert!(
            !payload.to_string().contains(PASSWORD),
            "a password is in the `{topic}` event"
        );
    }
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_marker_is_on_the_callers_own_session_only() {
    let Some(site) = Site::start().await else {
        return;
    };
    let first = site.login("administrator").await;
    let second = site.login("administrator").await;
    let reply = site.step_up(&first, json!({ "password": PASSWORD })).await;
    assert_eq!(reply.status, 200, "{}", reply.raw);
    assert_ne!(
        site.needs_step_up(&first).await.code(),
        "auth.step_up_required"
    );
    // A second session of the same user is not recent.
    let other = site.needs_step_up(&second).await;
    assert_eq!(other.status, 403, "{}", other.raw);
    assert_eq!(other.code(), "auth.step_up_required");
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_lapses_with_the_injected_clock() {
    let Some(site) = Site::start().await else {
        return;
    };
    let token = site.login("administrator").await;
    assert_eq!(
        site.step_up(&token, json!({ "password": PASSWORD }))
            .await
            .status,
        200
    );
    assert_ne!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    site.clock
        .advance(Duration::from_secs(u64::try_from(WINDOW).unwrap() + 1));
    let lapsed = site.needs_step_up(&token).await;
    assert_eq!(lapsed.status, 403, "{}", lapsed.raw);
    assert_eq!(lapsed.code(), "auth.step_up_required");
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_is_the_sign_in_answer_writes_no_marker_and_is_throttled_after_the_limit()
{
    // Five attempts per account: the sign-in below is the first, four wrong step-ups the next
    // four, and the sixth is over the limit.
    let Some(site) = Site::start_with(Some(5)).await else {
        return;
    };
    let token = site.login("administrator").await;
    let failed_before = site.events_of("identity_sign_in_failed").await.len();
    let mut first = None;
    for _ in 0..4 {
        let reply = site.step_up(&token, json!({ "password": WRONG })).await;
        assert_eq!(reply.status, 401, "{}", reply.raw);
        assert_eq!(reply.code(), "auth.invalid_credentials");
        assert!(!reply.raw.contains(WRONG));
        let seen = shape(&reply);
        assert_eq!(first.get_or_insert_with(|| seen.clone()), &seen);
    }
    // No marker was written.
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );

    // Over the limit: the rate-limit answer with Retry-After, and the credential is not looked at
    // (a right password is refused too, and still writes no marker).
    let throttled = site.step_up(&token, json!({ "password": PASSWORD })).await;
    assert_eq!(throttled.status, 429, "{}", throttled.raw);
    assert_eq!(throttled.code(), "api.rate_limited");
    assert!(
        throttled.header("retry-after").is_some(),
        "{}",
        throttled.raw
    );
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );

    // Every refusal is audited with the registry's reason class and no credential.
    let failed = site.events_of("identity_sign_in_failed").await;
    let new: Vec<&Json> = failed.iter().skip(failed_before).collect();
    assert_eq!(new.len(), 5, "{failed:?}");
    assert_eq!(
        new.iter()
            .filter(|d| d["reason"] == json!("bad_credentials"))
            .count(),
        4
    );
    assert_eq!(
        new.iter()
            .filter(|d| d["reason"] == json!("locked"))
            .count(),
        1
    );
    for data in new {
        assert_eq!(data["method"], "password");
        assert!(!data.to_string().contains(WRONG));
        assert!(!data.to_string().contains(PASSWORD));
    }
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_with_a_second_factor_needs_the_code_once_per_step() {
    let Some(site) = Site::start().await else {
        return;
    };
    let token = site.login("administrator").await;
    // Enrol and confirm a second factor through the second-factor routes.
    let enrol = site
        .call(
            Some(&token),
            "POST",
            "/api/v1/auth/totp/enrol",
            &[],
            Some(json!({})),
        )
        .await;
    assert_eq!(enrol.status, 200, "{}", enrol.raw);
    let secret = enrol.body["data"]["secret"].as_str().unwrap().to_owned();
    let confirm = site
        .call(
            Some(&token),
            "POST",
            "/api/v1/auth/totp/confirm",
            &[],
            Some(json!({ "code": totp_code(&secret, site.clock.now_utc().unix_timestamp()) })),
        )
        .await;
    assert_eq!(confirm.status, 200, "{}", confirm.raw);
    // The confirming code's step is spent: move on three steps, inside the window of the session.
    site.clock.advance(Duration::from_secs(90));
    let now = site.clock.now_utc().unix_timestamp();

    // The password alone is not the proof; the answer comes after the password was right.
    let missing = site.step_up(&token, json!({ "password": PASSWORD })).await;
    assert_eq!(missing.status, 401, "{}", missing.raw);
    assert_eq!(missing.code(), "auth.totp_required");
    let wrong = site
        .step_up(&token, json!({ "password": PASSWORD, "totp": "000000" }))
        .await;
    assert_eq!(wrong.status, 401, "{}", wrong.raw);
    assert_eq!(wrong.code(), "auth.totp_invalid");
    // A wrong password is the same answer whether or not the account has a second factor.
    let bad_with_factor = site
        .step_up(
            &token,
            json!({ "password": WRONG, "totp": totp_code(&secret, now) }),
        )
        .await;
    assert_eq!(bad_with_factor.code(), "auth.invalid_credentials");
    let bobby = site.login(BOBBY).await;
    let bad_without_factor = site.step_up(&bobby, json!({ "password": WRONG })).await;
    assert_eq!(shape(&bad_with_factor), shape(&bad_without_factor));
    // Nothing was written yet.
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );

    let ok = site
        .step_up(
            &token,
            json!({ "password": PASSWORD, "totp": totp_code(&secret, now) }),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
    assert_ne!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    // The same code in the same step is spent.
    let replay = site
        .step_up(
            &token,
            json!({ "password": PASSWORD, "totp": totp_code(&secret, now) }),
        )
        .await;
    assert_eq!(replay.status, 401, "{}", replay.raw);
    assert_eq!(replay.code(), "auth.totp_invalid");
    assert!(!replay.raw.contains(&totp_code(&secret, now)));
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_caller_is_refused_and_writes_nothing() {
    let Some(site) = Site::start().await else {
        return;
    };
    // Using an API key is not free of bookkeeping events of its own (the key's use is noted), so
    // what must not grow is the sign-in family of events a step-up writes.
    let sign_ins = |events: &[(String, Json)]| {
        events
            .iter()
            .filter(|(topic, _)| topic.starts_with("identity_sign_in"))
            .count()
    };
    let before = sign_ins(&site.events().await);
    let key = site.key.clone();
    let reply = site.step_up(&key, json!({ "password": PASSWORD })).await;
    assert_eq!(reply.status, 403, "{}", reply.raw);
    assert_eq!(reply.code(), "permission.denied");
    // An anonymous caller is refused at the door.
    let anonymous = site
        .call(
            None,
            "POST",
            "/api/v1/auth/step-up",
            &[],
            Some(json!({ "password": PASSWORD })),
        )
        .await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.raw);
    let after = site.events().await;
    assert_eq!(
        sign_ins(&after),
        before,
        "a step-up event was written: {:?}",
        after
            .iter()
            .map(|(topic, _)| topic.as_str())
            .collect::<Vec<_>>()
    );
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sso_is_not_configured_and_does_not_apply_to_a_password_session() {
    let Some(site) = Site::start().await else {
        return;
    };
    let token = site.login("administrator").await;
    // `sso: true` on a session a password started: the proof does not match the session.
    let reply = site.step_up(&token, json!({ "sso": true })).await;
    assert_eq!(reply.status, 400, "{}", reply.raw);
    assert_eq!(reply.code(), "api.invalid_request");
    // A malformed body never reaches the throttle or the password check.
    for body in [
        json!({}),
        json!({ "totp": "123456" }),
        json!({ "password": PASSWORD, "extra": 1 }),
    ] {
        let bad = site.step_up(&token, body).await;
        assert_eq!(bad.status, 400, "{}", bad.raw);
        assert_eq!(bad.code(), "api.invalid_request");
    }
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sso_administration_reads_the_marker_not_the_age_of_the_session() {
    // WP-IAM-12.T9: it used to read "the session was created within 15 minutes", so a fresh
    // sign-in counted as a step-up there although the identity administration refused it.
    let Some(site) =
        Site::start_with_extra(None, "[services.sso_identity]\nenabled = true\n").await
    else {
        return;
    };
    let token = site.login("administrator").await;
    // A minute old, never stepped up: not recent for the SSO administration either.
    site.clock.advance(Duration::from_secs(60));
    let before = site.sso_write(&token).await;
    assert_eq!(before.status, 403, "{}", before.raw);
    assert_eq!(before.code(), "auth.step_up_required");

    let reply = site.step_up(&token, json!({ "password": PASSWORD })).await;
    assert_eq!(reply.status, 200, "{}", reply.raw);
    let after = site.sso_write(&token).await;
    assert_ne!(after.code(), "auth.step_up_required", "{}", after.raw);

    // Another user's session is not recent because this one stepped up.
    let bobby = site.login(BOBBY).await;
    assert_eq!(site.sso_write(&bobby).await.code(), "auth.step_up_required");
    // And a second session of the same user is not either.
    let second = site.login("administrator").await;
    assert_eq!(
        site.sso_write(&second).await.code(),
        "auth.step_up_required"
    );

    // The marker lapses with the injected clock, by the same window the identity administration
    // uses (`step_up_window_secs`, 60 s here).
    site.clock
        .advance(Duration::from_secs(u64::try_from(WINDOW).unwrap() + 1));
    let lapsed = site.sso_write(&token).await;
    assert_eq!(lapsed.status, 403, "{}", lapsed.raw);
    assert_eq!(lapsed.code(), "auth.step_up_required");
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    site.finish().await;
}
