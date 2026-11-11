//! WP-IAM-07.T2: a user lists and ends their own sessions (`auth_session_list`,
//! `auth_session_end`). Real PostgreSQL, a real `spot serve`, real HTTP (skipped without
//! `SPOT_TEST_PG_URL`).
//!
//! Security properties proved here: a user sees and ends only their own sessions; another user's
//! id and an unknown id are one and the same answer; no token or digest is ever returned; ending
//! the current session works and invalidates it at once; an API key and a first-sign-in session
//! are refused (denied, never an empty list); every ending is audited, fail closed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::audit::{CapturingAudit, IdentityAudit};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use support::{Env, run};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const BOBBY: &str = "bobby@example.com";

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

async fn login(addr: SocketAddr, host: &str, user: &str) -> String {
    let reply = http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": user, "password": PASSWORD})),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.raw);
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

async fn sessions(addr: SocketAddr, host: &str, token: &str) -> Reply {
    http(
        addr,
        host,
        Some(token),
        "GET",
        "/api/v1/auth/sessions",
        None,
    )
    .await
}

async fn end(addr: SocketAddr, host: &str, token: &str, id: &str) -> Reply {
    http(
        addr,
        host,
        Some(token),
        "DELETE",
        &format!("/api/v1/auth/sessions/{id}"),
        None,
    )
    .await
}

async fn whoami(addr: SocketAddr, host: &str, token: &str) -> Reply {
    http(addr, host, Some(token), "GET", "/api/v1/auth/session", None).await
}

fn ids(reply: &Reply) -> Vec<String> {
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

fn current_id(reply: &Reply) -> String {
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == json!(true))
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .unwrap()
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
    audit: Arc<CapturingAudit>,
    clock: Arc<spot_core::ManualClock>,
    /// The Administrator's API key, when the site was created with one.
    key: String,
}

impl Site {
    async fn start() -> Option<Site> {
        let env = Env::new().await?;
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
        let audit = Arc::new(CapturingAudit::new());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let options = Options {
            clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
            // The authenticator's session cache stays at its default: ending a session must
            // invalidate it, not wait for it to lapse.
            login: LoginConfig::for_tests(),
        };
        let sink: Arc<dyn IdentityAudit> = audit.clone();
        let server = spot_cli::start_with_audit(&env.root, false, options, Some(sink), async {
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
            audit,
            clock,
            key,
        })
    }

    fn host(&self) -> String {
        self.env.site.clone()
    }

    async fn login(&self, user: &str) -> String {
        // One second apart, so `created` orders the sessions.
        self.clock.advance(Duration::from_secs(1));
        login(self.addr, &self.host(), user).await
    }

    async fn sessions(&self, token: &str) -> Reply {
        sessions(self.addr, &self.host(), token).await
    }

    async fn end(&self, token: &str, id: &str) -> Reply {
        end(self.addr, &self.host(), token, id).await
    }

    async fn whoami(&self, token: &str) -> Reply {
        whoami(self.addr, &self.host(), token).await
    }

    async fn finish(self) {
        self.stop.send(()).unwrap();
        self.server.handle.join().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_lists_their_own_live_sessions_current_first_then_newest_first() {
    let Some(site) = Site::start().await else {
        return;
    };
    let first = site.login("administrator").await;
    let second = site.login("administrator").await;
    let third = site.login("administrator").await;

    let listed = site.sessions(&second).await;
    assert_eq!(listed.status, 200, "{}", listed.raw);
    let records = listed.body["data"].as_array().unwrap();
    assert_eq!(records.len(), 3, "{}", listed.raw);
    // The caller's session first, then the others newest first.
    assert_eq!(records[0]["current"], json!(true));
    assert_eq!(records[1]["current"], json!(false));
    assert_eq!(records[2]["current"], json!(false));
    let created: Vec<&str> = records[1..]
        .iter()
        .map(|r| r["created_at"].as_str().unwrap())
        .collect();
    assert!(created[0] > created[1], "newest first: {created:?}");

    // The contract's record, and nothing that is a credential.
    for record in records {
        let mut keys: Vec<&str> = record
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        for key in keys {
            assert!(
                [
                    "id",
                    "created_at",
                    "expires_at",
                    "last_used_at",
                    "method",
                    "source",
                    "device",
                    "current"
                ]
                .contains(&key),
                "unexpected field {key}"
            );
        }
        assert_eq!(record["method"], "password");
        assert!(record["id"].as_str().unwrap().len() <= 64);
    }
    for token in [&first, &second, &third] {
        assert!(
            !listed.raw.contains(token.as_str()),
            "a token is in the session list"
        );
    }
    // The current flag follows the caller.
    let from_third = site.sessions(&third).await;
    assert_ne!(current_id(&listed), current_id(&from_third));
    assert_eq!(from_third.body["data"][0]["current"], json!(true));
    let mut a = ids(&listed);
    let mut b = ids(&from_third);
    a.sort();
    b.sort();
    assert_eq!(a, b, "the same three sessions from either one");

    // Unauthenticated and a bad token: refused, never an empty list.
    let anonymous = http(
        site.addr,
        &site.host(),
        None,
        "GET",
        "/api/v1/auth/sessions",
        None,
    )
    .await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.raw);
    let bad = site.sessions("not-a-token").await;
    assert_eq!(bad.status, 401, "{}", bad.raw);
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_users_session_id_answers_exactly_like_an_unknown_one() {
    let Some(site) = Site::start().await else {
        return;
    };
    let admin = site.login("administrator").await;
    let bobby = site.login(BOBBY).await;
    let admin_list = site.sessions(&admin).await;
    let bobby_list = site.sessions(&bobby).await;
    assert_eq!(admin_list.status, 200, "{}", admin_list.raw);
    assert_eq!(bobby_list.status, 200, "{}", bobby_list.raw);
    let bobby_id = current_id(&bobby_list);
    let admin_id = current_id(&admin_list);
    // Each sees only their own.
    assert_eq!(ids(&admin_list), vec![admin_id.clone()]);
    assert_eq!(ids(&bobby_list), vec![bobby_id.clone()]);
    assert!(!admin_list.raw.contains(&bobby_id));
    assert!(!bobby_list.raw.contains(&admin_id));

    // The administrator tries bobby's id, a random one, and one of the same shape that never
    // existed: all three are the same answer.
    let foreign = site.end(&admin, &bobby_id).await;
    let unknown = site.end(&admin, "never-existed").await;
    let shaped = site.end(&admin, &"a".repeat(bobby_id.len())).await;
    assert_eq!(foreign.status, 404, "{}", foreign.raw);
    assert_eq!(foreign.body["error"]["code"], "auth.session_not_found");
    assert_eq!(shape(&foreign), shape(&unknown));
    assert_eq!(shape(&foreign), shape(&shaped));

    // And the other way round, and the foreign session is untouched.
    let reverse = site.end(&bobby, &admin_id).await;
    assert_eq!(shape(&reverse), shape(&unknown));
    assert_eq!(site.whoami(&bobby).await.status, 200);
    assert_eq!(site.whoami(&admin).await.status, 200);
    assert_eq!(ids(&site.sessions(&bobby).await), vec![bobby_id]);
    // The foreign attempt is not an ending: no ending was audited.
    assert!(site.audit.of("session_ended_by_user").is_empty());

    // A malformed id is a shape error for everyone, before any lookup.
    let malformed = site.end(&admin, "has.dot").await;
    assert_eq!(malformed.status, 400, "{}", malformed.raw);
    assert_eq!(malformed.body["error"]["code"], "api.invalid_request");
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_another_own_session_stops_it_at_once_and_leaves_the_rest() {
    let Some(site) = Site::start().await else {
        return;
    };
    let keep = site.login("administrator").await;
    let doomed = site.login("administrator").await;
    let other = site.login("administrator").await;
    // Warm the authenticator cache for the session about to end.
    assert_eq!(site.whoami(&doomed).await.status, 200);
    let doomed_id = current_id(&site.sessions(&doomed).await);

    let ended = site.end(&keep, &doomed_id).await;
    assert_eq!(ended.status, 204, "{}", ended.raw);
    assert!(ended.raw.ends_with("\r\n\r\n"), "no body: {}", ended.raw);

    // The very next request with the ended token is refused, cache or not.
    let after = site.whoami(&doomed).await;
    assert_eq!(after.status, 401, "{}", after.raw);
    let listing = site.sessions(&doomed).await;
    assert_eq!(listing.status, 401, "{}", listing.raw);
    // The caller's own and the third session still work; the list no longer has the ended one.
    assert_eq!(site.whoami(&keep).await.status, 200);
    assert_eq!(site.whoami(&other).await.status, 200);
    let remaining = ids(&site.sessions(&keep).await);
    assert_eq!(remaining.len(), 2);
    assert!(!remaining.contains(&doomed_id));

    // Ending it again is the one not-found answer (the session is gone), never a 500.
    let again = site.end(&keep, &doomed_id).await;
    assert_eq!(again.status, 404, "{}", again.raw);
    assert_eq!(again.body["error"]["code"], "auth.session_not_found");

    // The ending is audited: who, which session, why; and no token anywhere in any record.
    let records = site.audit.of("session_ended_by_user");
    assert_eq!(records.len(), 1, "{:?}", site.audit.records());
    assert_eq!(records[0].session.as_deref(), Some(doomed_id.as_str()));
    assert_eq!(records[0].reason.as_deref(), Some("user_ended"));
    assert!(records[0].user.is_some());
    let everything = format!("{:?}", site.audit.records());
    for token in [&keep, &doomed, &other] {
        assert!(!everything.contains(token.as_str()), "a token is audited");
    }
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_the_current_session_works_and_equals_logout() {
    let Some(site) = Site::start().await else {
        return;
    };
    let current = site.login("administrator").await;
    let other = site.login("administrator").await;
    assert_eq!(site.whoami(&current).await.status, 200);
    let id = current_id(&site.sessions(&current).await);

    let ended = site.end(&current, &id).await;
    assert_eq!(ended.status, 204, "{}", ended.raw);
    // The token no longer authenticates on the next request, on any route.
    assert_eq!(site.whoami(&current).await.status, 401);
    assert_eq!(site.sessions(&current).await.status, 401);
    assert_eq!(
        site.end(&current, &id).await.status,
        401,
        "an ended token cannot end anything"
    );
    // The other session is unaffected and no longer lists it.
    assert!(!ids(&site.sessions(&other).await).contains(&id));
    // Audited once, with the reason.
    let records = site.audit.of("session_ended_by_user");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reason.as_deref(), Some("user_ended"));
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ending_that_cannot_be_audited_is_refused_and_the_session_stays() {
    let Some(site) = Site::start().await else {
        return;
    };
    let caller = site.login("administrator").await;
    let victim = site.login("administrator").await;
    let victim_id = current_id(&site.sessions(&victim).await);

    // Both attempts (the write and its retry) fail: the act is refused.
    site.audit.fail_next(2);
    let refused = site.end(&caller, &victim_id).await;
    assert_eq!(refused.status, 503, "{}", refused.raw);
    assert_eq!(refused.body["error"]["code"], "server.unavailable");
    assert_eq!(
        site.whoami(&victim).await.status,
        200,
        "an ending that could not be audited did not happen"
    );
    // Once the audit works again it succeeds.
    let ok = site.end(&caller, &victim_id).await;
    assert_eq!(ok.status, 204, "{}", ok.raw);
    assert_eq!(site.whoami(&victim).await.status, 401);
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_cannot_list_or_end_sessions() {
    let Some(site) = Site::start().await else {
        return;
    };
    let session = site.login("administrator").await;
    let id = current_id(&site.sessions(&session).await);

    let listed = site.sessions(&site.key).await;
    assert_eq!(listed.status, 403, "{}", listed.raw);
    assert_eq!(listed.body["error"]["code"], "permission.denied");
    assert!(
        listed.body.get("data").is_none(),
        "denied, not an empty success"
    );
    let ended = site.end(&site.key, &id).await;
    assert_eq!(ended.status, 403, "{}", ended.raw);
    assert_eq!(ended.body["error"]["code"], "permission.denied");
    // The session the key's owner holds is untouched.
    assert_eq!(site.whoami(&session).await.status, 200);

    // Recorded security finding: reading the session with an API key must not be a server error.
    let read = site.whoami(&site.key).await;
    assert_ne!(read.status, 500, "{}", read.raw);
    assert!(
        matches!(read.status, 200 | 401 | 403),
        "an API key reading the session answers a refusal or the identity: {}",
        read.raw
    );
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sign_in_session_cannot_manage_sessions() {
    let Some(env) = Env::new().await else { return };
    let out = env.ok(&["new-site", &env.site]);
    let secret = out.trim().rsplit(' ').next().unwrap().to_owned();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: None,
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    let host = env.site.clone();
    let first = http(
        addr,
        &host,
        None,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": "administrator", "password": secret})),
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let restricted = first.body["data"]["token"].as_str().unwrap().to_owned();
    let listed = sessions(addr, &host, &restricted).await;
    assert_eq!(listed.status, 403, "{}", listed.raw);
    assert!(listed.body.get("data").is_none());
    let ended = end(addr, &host, &restricted, "anything").await;
    assert_eq!(ended.status, 403, "{}", ended.raw);
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_password_change_still_keeps_the_callers_session_and_ends_the_rest() {
    let Some(site) = Site::start().await else {
        return;
    };
    let caller = site.login("administrator").await;
    let other = site.login("administrator").await;
    let changed = http(
        site.addr,
        &site.host(),
        Some(&caller),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": PASSWORD, "new": "another-violet-staple-7-Quartz"})),
    )
    .await;
    assert_eq!(changed.status, 204, "{}", changed.raw);
    // The T6 rule is untouched: the caller keeps theirs, the other is gone.
    assert_eq!(site.whoami(&caller).await.status, 200);
    assert_eq!(site.whoami(&other).await.status, 401);
    assert_eq!(ids(&site.sessions(&caller).await).len(), 1);
    site.finish().await;
}
