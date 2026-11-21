//! WP-IAM-12.T9: a step-up is a record of the site's identity audit chain, like a sign-in. A
//! success is written (required) **before** the session's marker, so an unauditable step-up is
//! refused and leaves no marker; a refusal is written with its reason class and no marker. No
//! record holds a password, a code or a token. Real PostgreSQL, a real `spot serve`, real HTTP
//! (skipped without `SPOT_TEST_PG_URL`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::audit::chain::{IdentityChain, scope_of};
use spot_cli::audit::{CapturingAudit, IdentityAudit};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use support::{Env, run};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const WRONG: &str = "not-the-password-at-all-1234";

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

struct Site {
    env: Env,
    server: Running,
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    clock: Arc<spot_core::ManualClock>,
}

impl Site {
    /// A site with a one-minute step-up window. With `sink` the identity audit goes to that
    /// in-memory sink (which can be told to fail); without it the host writes its own chain.
    async fn start(sink: Option<Arc<CapturingAudit>>) -> Option<Site> {
        let env = Env::new().await?;
        let common = env.root.join("common.toml");
        let mut text = std::fs::read_to_string(&common).unwrap();
        text.push_str("[services.identity_admin]\nstep_up_window_secs = 60\n");
        std::fs::write(&common, text).unwrap();
        let key_file = env.root.join("admin.key");
        env.ok(&[
            "new-site",
            &env.site,
            "--admin-api-key-file",
            key_file.to_str().unwrap(),
        ]);
        let output = run(
            &env.root,
            &[
                "set-password",
                &env.site,
                "administrator",
                "--password-stdin",
            ],
            Some(&format!("{PASSWORD}\n")),
        );
        assert!(output.status.success(), "{}", support::text(&output));
        let clock = Arc::new(spot_core::ManualClock::new(time::OffsetDateTime::now_utc()));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let options = Options {
            clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
            login: LoginConfig::for_tests(),
        };
        let sink = sink.map(|s| s as Arc<dyn IdentityAudit>);
        let server = spot_cli::start_with_audit(&env.root, false, options, sink, async {
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
    async fn login(&self) -> String {
        self.clock.advance(Duration::from_secs(1));
        let reply = self
            .call(
                None,
                "POST",
                "/api/v1/auth/login",
                &[],
                Some(json!({"user": "administrator", "password": PASSWORD})),
            )
            .await;
        assert_eq!(reply.status, 200, "{}", reply.raw);
        reply.body["data"]["token"].as_str().unwrap().to_owned()
    }

    async fn step_up(&self, token: &str, password: &str) -> Reply {
        self.call(
            Some(token),
            "POST",
            "/api/v1/auth/step-up",
            &[],
            Some(json!({ "password": password })),
        )
        .await
    }

    /// An operation that needs a recent step-up (creating an API key): the step-up check comes
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

    /// The identity chain's stored records, oldest first, and whether the chain verifies.
    async fn chain(&self) -> (Vec<String>, bool) {
        let loaded = spot_cli::config::load(&self.env.root, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
        let scope = scope_of(&self.env.site);
        let report = IdentityChain::new(&backend, &scope).verify().await.unwrap();
        let client = backend.pool().get().await.unwrap();
        let rows = client
            .query(
                "SELECT record FROM spot_audit_chain WHERE site = $1 ORDER BY seq",
                &[&scope],
            )
            .await
            .unwrap();
        (
            rows.iter().map(|r| r.get::<_, String>(0)).collect(),
            report.is_ok(),
        )
    }

    async fn finish(self) {
        self.stop.send(()).unwrap();
        self.server.handle.join().await.unwrap();
    }
}

/// Nothing a caller supplied or was given may be in a record.
fn assert_no_secret(text: &str, token: &str) {
    for needle in [PASSWORD, WRONG, token] {
        assert!(!text.contains(needle), "`{needle}` reached the audit");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_password_step_up_is_one_record_in_the_identity_chain_and_the_chain_verifies() {
    let Some(site) = Site::start(None).await else {
        return;
    };
    let token = site.login().await;
    let (before, _) = site.chain().await;
    let count = |records: &[String], kind: &str| {
        records
            .iter()
            .filter(|r| r.contains(&format!("identity.{kind}")))
            .count()
    };
    let succeeded_before = count(&before, "step_up_succeeded");

    let reply = site.step_up(&token, PASSWORD).await;
    assert_eq!(reply.status, 200, "{}", reply.raw);

    let (after, verifies) = site.chain().await;
    assert!(verifies, "the identity chain does not verify");
    assert_eq!(
        count(&after, "step_up_succeeded"),
        succeeded_before + 1,
        "{after:?}"
    );
    // The record names the channel and carries no free text.
    let record = after
        .iter()
        .rev()
        .find(|r| r.contains("identity.step_up_succeeded"))
        .unwrap();
    assert!(
        record.contains("channel") && record.contains("api"),
        "{record}"
    );
    assert_no_secret(&after.join("\n"), &token);
    // The step-up took effect.
    assert_ne!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_is_a_failed_record_with_its_reason_class_and_writes_no_marker() {
    let Some(site) = Site::start(None).await else {
        return;
    };
    let token = site.login().await;
    let reply = site.step_up(&token, WRONG).await;
    assert_eq!(reply.status, 401, "{}", reply.raw);
    assert_eq!(reply.code(), "auth.invalid_credentials");

    let (records, verifies) = site.chain().await;
    assert!(verifies);
    let failed: Vec<&String> = records
        .iter()
        .filter(|r| r.contains("identity.step_up_failed"))
        .collect();
    assert_eq!(failed.len(), 1, "{records:?}");
    assert!(failed[0].contains("bad_credentials"), "{}", failed[0]);
    assert!(
        !records
            .iter()
            .any(|r| r.contains("identity.step_up_succeeded")),
        "a refusal is not recorded as a success"
    );
    assert_no_secret(&records.join("\n"), &token);
    // No marker: the session is still not recent.
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_up_whose_record_cannot_be_written_is_refused_and_leaves_no_marker() {
    let sink = Arc::new(CapturingAudit::new());
    let Some(site) = Site::start(Some(Arc::clone(&sink))).await else {
        return;
    };
    // The sign-in is itself audited fail-closed: sign in while the sink works.
    let token = site.login().await;
    sink.fail_all(true);
    let refused = site.step_up(&token, PASSWORD).await;
    assert_eq!(refused.status, 503, "{}", refused.raw);
    assert_eq!(refused.code(), "server.unavailable");
    assert!(sink.of("step_up_succeeded").is_empty());
    // No marker was written, so nothing that needs a step-up is allowed.
    sink.fail_all(false);
    assert_eq!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );

    // Once the sink writes again the same step-up succeeds and is recorded.
    let ok = site.step_up(&token, PASSWORD).await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
    let written = sink.of("step_up_succeeded");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].channel.as_deref(), Some("api"));
    assert!(written[0].user.is_some() && written[0].session.is_some());
    assert!(written[0].reason.is_none());
    assert_ne!(
        site.needs_step_up(&token).await.code(),
        "auth.step_up_required"
    );
    // Nothing a caller sent or was given is in any record.
    let text = serde_json::to_string(&sink.records()).unwrap();
    assert_no_secret(&text, &token);
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_sink_never_changes_the_answer_to_a_wrong_password() {
    let sink = Arc::new(CapturingAudit::new());
    let Some(site) = Site::start(Some(Arc::clone(&sink))).await else {
        return;
    };
    let token = site.login().await;
    let before = site.step_up(&token, WRONG).await;
    sink.fail_all(true);
    let during = site.step_up(&token, WRONG).await;
    // The same answer with the audit down: an outage is not an oracle.
    assert_eq!(before.status, during.status, "{}", during.raw);
    assert_eq!(before.code(), during.code());
    let failed = sink.of("step_up_failed");
    assert_eq!(failed.len(), 1, "only the first was written");
    assert_eq!(failed[0].reason.as_deref(), Some("bad_credentials"));
    site.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_caller_is_recorded_as_not_a_person() {
    let sink = Arc::new(CapturingAudit::new());
    let Some(site) = Site::start(Some(Arc::clone(&sink))).await else {
        return;
    };
    let key = std::fs::read_to_string(site.env.root.join("admin.key"))
        .unwrap()
        .trim()
        .to_owned();
    let reply = site.step_up(&key, PASSWORD).await;
    assert_eq!(reply.status, 403, "{}", reply.raw);
    assert_eq!(reply.code(), "permission.denied");
    let failed = sink.of("step_up_failed");
    assert_eq!(failed.len(), 1, "{:?}", sink.records());
    assert_eq!(failed[0].reason.as_deref(), Some("not_a_person"));
    assert!(sink.of("step_up_succeeded").is_empty());
    for record in sink.records() {
        assert_no_secret(&format!("{record:?}"), &key);
    }
    site.finish().await;
}
