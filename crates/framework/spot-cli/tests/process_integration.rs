//! Integration tests for the Integration-node hosting module (`services::process_integration`,
//! WP-BPM-06.T3), driven through `register_with` (the body `register()` runs) over a real
//! `SiteCtx`: dispatch by `connector`, the hosted egress client's SSRF, rebinding and redirect
//! defences, vault failure, redaction, timeout and the site's retry cap, and the mail queue's
//! durable send keyed by `(case, node, attempt)`. Every network-touching test runs its own
//! loopback server; nothing reaches the real network.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_bpm_engine::{CaseId, IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use spot_bpm_integration::{
    HttpNodeHandler, InMemorySecretResolver, IntegrationRegistry, MailPort as _, RetryClass,
    WebhookNodeHandler,
};
use spot_cli::config::Mail;
use spot_cli::services::SiteCtx;
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::HandlerBuilder;
use spot_cli::services::process_integration::{
    EngineIntegrationHandler, QueueMailPort, RawWebhookPort, SmtpMailPort, Wiring,
    integration_egress_policy, register, register_with, run_send_mail_job, sealed_resolver,
};
use spot_core::{Clock, SiteId, SystemClock};
use spot_dag::NodeId;
use spot_egress::{
    AllowEntry, EgressConnector, EgressPolicy, Guard, HttpSender, InMemorySink, Purpose, Sender,
    TenantPolicy, TestResolver,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CANARY: &str = "canary-secret-value-0123456789";

struct Server {
    port: u16,
    connections: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
}

/// Reads one whole request (head, then `content-length` bytes), so a reply never races a client
/// that writes its header and body separately.
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let (mut data, mut chunk) = (Vec::new(), [0u8; 4096]);
    loop {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
        let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
        let wanted = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if data.len() >= end + 4 + wanted {
            break;
        }
    }
    String::from_utf8_lossy(&data).to_string()
}

/// A server answering each request with `reply(request)`; `None` never answers (a call that
/// times out).
async fn serve_with(reply: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (c, h, reply) = (connections.clone(), heads.clone(), Arc::new(reply));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            c.fetch_add(1, Ordering::SeqCst);
            let (h, reply) = (h.clone(), reply.clone());
            tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let response = reply(&request);
                h.lock().unwrap().push(request);
                match response {
                    Some(response) => {
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            });
        }
    });
    Server {
        port,
        connections,
        heads,
    }
}

async fn serve(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> Server {
    serve_with(move |request| Some(reply(request))).await
}

fn ok(body: &str) -> String {
    let len = body.len();
    format!("HTTP/1.1 200 OK\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{body}")
}

fn conns(server: &Server) -> usize {
    server.connections.load(Ordering::SeqCst)
}

/// Behaves like the job store: a job is kept only when its idempotency key is new; every
/// submission is counted.
#[derive(Default)]
struct RecordingSubmit {
    jobs: Mutex<Vec<JobSpec>>,
    submissions: AtomicUsize,
}

impl JobSubmit for RecordingSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            self.submissions.fetch_add(1, Ordering::SeqCst);
            let mut jobs = self.jobs.lock().unwrap();
            let fresh = !jobs
                .iter()
                .any(|j| j.idempotency_key == job.idempotency_key);
            if fresh {
                jobs.push(job);
            }
            Ok(fresh)
        })
    }
}

impl RecordingSubmit {
    fn keys(&self) -> Vec<String> {
        let jobs = self.jobs.lock().unwrap();
        jobs.iter().map(|j| j.idempotency_key.clone()).collect()
    }
}

/// A site assembled through `register_with`, with the process module's `HandlerBuilder` port and
/// a recording job store provided, as the services registry would.
struct Site {
    handler: Arc<EngineIntegrationHandler>,
    submit: Arc<RecordingSubmit>,
    ctx: SiteCtx<'static>,
}

fn new_ctx(
    tenant: &str,
    settings: &str,
    assembling: bool,
) -> (SiteCtx<'static>, Arc<RecordingSubmit>) {
    let mut table = BTreeMap::new();
    if !settings.is_empty() {
        let value = toml::from_str::<toml::Value>(settings).unwrap();
        table.insert("process_integration".to_owned(), value);
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut ctx = SiteCtx::new(tenant, Duration::from_secs(1), clock, table, assembling);
    let submit = Arc::new(RecordingSubmit::default());
    ctx.provide(Arc::new(HandlerBuilder::new())).unwrap();
    ctx.provide::<dyn JobSubmit>(submit.clone() as Arc<dyn JobSubmit>)
        .unwrap();
    (ctx, submit)
}

/// The default policy plus: `svc.test` on `port` may resolve to loopback for `tenant`.
fn policy_allowing(tenant: &str, port: Option<u16>) -> EgressPolicy {
    let mut policy = integration_egress_policy();
    if let Some(port) = port {
        let allow = AllowEntry {
            scheme: Some("http".to_owned()),
            host: "svc.test".to_owned(),
            ports: Some(vec![port]),
            purpose: None,
            allow_private: Some(vec!["127.0.0.1".to_owned()]),
        };
        // The derived `Default` has a connect timeout of 0 s (only serde's default is
        // non-zero), which would fail a connect that needs a second poll: set it.
        let tenant_policy = TenantPolicy {
            allow: vec![allow],
            connect_timeout_secs: 5,
            ..TenantPolicy::default()
        };
        policy.tenants.insert(tenant.to_owned(), tenant_policy);
    }
    policy
}

fn resolver_for(host: &str, ip: &str) -> Arc<TestResolver> {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(host, vec![vec![ip.parse().unwrap()]; 16]);
    resolver
}

async fn site_with(
    tenant: &str,
    port: Option<u16>,
    resolver: Arc<TestResolver>,
    settings: &str,
    mail: &Mail,
) -> Site {
    let (mut ctx, submit) = new_ctx(tenant, settings, true);
    let wiring = Wiring {
        resolver,
        policy: policy_allowing(tenant, port),
    };
    let site_id = SiteId::new(tenant).unwrap();
    let registered = register_with(&mut ctx, &site_id, mail, wiring)
        .await
        .unwrap();
    let handler = registered.expect("an enabled module registers a handler");
    Site {
        handler,
        submit,
        ctx,
    }
}

/// A site whose `svc.test` resolves to loopback and may be reached on `port`.
async fn site(tenant: &str, port: u16, settings: &str) -> Site {
    let resolver = resolver_for("svc.test", "127.0.0.1");
    site_with(tenant, Some(port), resolver, settings, &Mail::default()).await
}

/// A site with no allow entry and no names the resolver knows.
async fn bare_site(tenant: &str, resolver: Arc<TestResolver>) -> Site {
    site_with(tenant, None, resolver, "", &Mail::default()).await
}

fn run_node_for(
    handler: &EngineIntegrationHandler,
    config: &Json,
    case: &str,
    attempt: u32,
) -> NodeOutcome {
    let case = CaseId::new(case).unwrap();
    let node = NodeId::new("n1").unwrap();
    let key = IdempotencyKey::new(case.clone(), node.clone(), attempt);
    let mut variables = json!({});
    let mut ctx = NodeCtx {
        case: &case,
        node: &node,
        kind: NodeKind::Integration,
        attempt,
        key,
        is_retry: false,
        now: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        deadline: None,
        principal: None,
        config,
        outgoing: &[],
        variables: &mut variables,
        cancelled: false,
    };
    handler.run(&mut ctx)
}

fn has_job(ctx: &mut SiteCtx<'_>) -> bool {
    let jobs = ctx.take_job_handlers();
    jobs.get("bpm_integration:send_mail").is_some()
}

fn run(handler: &EngineIntegrationHandler, config: &Json) -> NodeOutcome {
    run_node_for(handler, config, "case1", 1)
}

fn node(connector: &str, endpoint: &Json) -> Json {
    json!({ "connector": connector, "action": "x", "endpoint": endpoint.to_string() })
}

fn http(url: &str) -> Json {
    node("http", &json!({ "url": url }))
}

fn code_of(outcome: &NodeOutcome) -> String {
    match outcome {
        NodeOutcome::Fail { code, .. } => code.clone(),
        NodeOutcome::Retry { .. } => "retry".to_owned(),
        other => format!("not a failure: {other:?}"),
    }
}

fn is_done(outcome: &NodeOutcome) -> bool {
    matches!(outcome, NodeOutcome::Done { .. })
}

/// A handler over a hand-built registry (for the tests that need their own secret resolver).
fn http_registry(tenant: &str, port: u16) -> IntegrationRegistry {
    let policy = policy_allowing(tenant, Some(port));
    let resolver = resolver_for("svc.test", "127.0.0.1");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let guard = Arc::new(Guard::new(
        policy,
        resolver,
        Arc::new(InMemorySink::new()),
        clock,
    ));
    let connector = |purpose| EgressConnector::new(Arc::clone(&guard), tenant, purpose);
    let sender = |purpose| -> Arc<dyn Sender> {
        Arc::new(HttpSender::new(connector(purpose), Duration::from_secs(5)))
    };
    let port_http = spot_bpm_integration::SpotEgressPort::new(
        Arc::clone(&guard),
        sender(Purpose::Connector),
        Purpose::Connector,
        None,
        1024 * 1024,
    );
    let webhook = RawWebhookPort::new(
        Arc::clone(&guard),
        sender(Purpose::Webhook),
        tenant,
        1024 * 1024,
    );
    let mut registry = IntegrationRegistry::new();
    registry.register(HttpNodeHandler::new(Arc::new(port_http), tenant));
    registry.register(WebhookNodeHandler::new(Arc::new(webhook)));
    registry
}

#[tokio::test(flavor = "multi_thread")]
async fn register_is_inert_disabled_or_reloaded_and_needs_a_site_environment_otherwise() {
    let (mut ctx, _) = new_ctx("acme", "enabled = false", true);
    register(&mut ctx).await.unwrap();
    assert!(!has_job(&mut ctx));
    // Enabled with no site environment (a context built for a test): refused, not guessed.
    let (mut ctx, _) = new_ctx("acme", "", true);
    assert!(register(&mut ctx).await.is_err());
    // A reload (not assembling) builds no second copy.
    let (mut ctx, _) = new_ctx("acme", "", false);
    register(&mut ctx).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn register_with_refuses_bad_settings_and_a_missing_process_port() {
    let site_id = SiteId::new("acme").unwrap();
    let bad = ["max_retry_attempts = 0", "enabedl = true"];
    for settings in bad {
        let (mut ctx, _) = new_ctx("acme", settings, true);
        let mail = Mail::default();
        let result = register_with(&mut ctx, &site_id, &mail, Wiring::production());
        assert!(result.await.is_err(), "{settings} must be refused");
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock, BTreeMap::new(), true);
    let mail = Mail::default();
    let result = register_with(&mut ctx, &site_id, &mail, Wiring::production());
    assert!(
        result.await.is_err(),
        "the process module's builder port is required"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn register_with_hosts_the_integration_kind_and_the_mail_job() {
    let mut s = bare_site("acme", Arc::new(TestResolver::new())).await;
    assert_eq!(s.handler.kind(), NodeKind::Integration);
    assert!(has_job(&mut s.ctx));

    // Mail disabled: no job handler, and a mail node fails closed rather than dropping its mail.
    let off: Mail = toml::from_str("enabled = false").unwrap();
    let resolver = Arc::new(TestResolver::new());
    let mut s = site_with("acme", None, resolver, "", &off).await;
    assert!(!has_job(&mut s.ctx));
    let outcome = run(&s.handler, &node("mail", &mail_endpoint()));
    assert_eq!(code_of(&outcome), "bpm_integration.mail_permanent");
    assert_eq!(s.submit.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_naming_nothing_hostable_fails_its_config_before_any_call() {
    let s = bare_site("acme", Arc::new(TestResolver::new())).await;
    let unknown = node("wasm:as2", &json!({}));
    let bad_endpoint = json!({ "connector": "http", "action": "x", "endpoint": "not json" });
    let no_endpoint = json!({ "connector": "http", "action": "x" });
    let no_connector = json!({ "action": "x", "endpoint": "{}" });
    for config in [unknown, bad_endpoint, no_endpoint, no_connector] {
        let outcome = run(&s.handler, &config);
        assert_eq!(
            code_of(&outcome),
            "bpm_integration.config_invalid",
            "{config}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn loopback_link_local_and_metadata_destinations_are_refused_with_the_egress_code() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response("evil.test", vec![vec!["127.0.0.1".parse().unwrap()]]);
    resolver.add_response("meta.test", vec![vec!["169.254.169.254".parse().unwrap()]]);
    resolver.add_response("link.test", vec![vec!["169.254.10.10".parse().unwrap()]]);
    // The test resolver also answers for an address literal (the system one returns it as is).
    for literal in [
        "169.254.1.1",
        "169.254.169.254",
        "::1",
        "127.0.0.1",
        "10.0.0.5",
    ] {
        resolver.add_response(literal, vec![vec![literal.parse().unwrap()]]);
    }
    resolver.add_response("[::1]", vec![vec!["::1".parse().unwrap()]]);
    let s = bare_site("acme", resolver).await;
    let urls = [
        "http://169.254.1.1/x",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/x",
        "http://127.0.0.1/x",
        "http://10.0.0.5/x",
        "http://evil.test/x",
        "http://meta.test/x",
        "http://link.test/x",
    ];
    for url in urls {
        let outcome = run(&s.handler, &http(url));
        assert!(
            matches!(outcome, NodeOutcome::Fail { .. }),
            "{url}: refused, not retried"
        );
        assert_eq!(code_of(&outcome), "bpm_integration.egress_denied", "{url}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dns_rebinding_cannot_steer_a_vetted_connection_or_pass_the_next_lookup() {
    let server = serve(|_| ok("{}")).await;
    // First lookup: the loopback address this tenant may reach. Later lookups: the metadata
    // address (what a rebinding attacker flips the name to).
    let resolver = Arc::new(TestResolver::new());
    let flipped = vec!["169.254.169.254".parse().unwrap()];
    let answers = vec![vec!["127.0.0.1".parse().unwrap()], flipped.clone(), flipped];
    resolver.add_response("svc.test", answers);
    let s = site_with(
        "acme",
        Some(server.port),
        resolver.clone(),
        "",
        &Mail::default(),
    )
    .await;
    let config = http(&format!("http://svc.test:{}/x", server.port));

    // One lookup per request, and the connection goes only where that lookup was vetted.
    assert!(is_done(&run(&s.handler, &config)));
    assert_eq!(resolver.call_count("svc.test"), 1);
    assert_eq!(conns(&server), 1);
    // The next request is vetted afresh, meets the rebound answer and is refused unconnected.
    let second = run(&s.handler, &config);
    assert_eq!(
        code_of(&second),
        "bpm_integration.egress_denied",
        "{second:?}"
    );
    assert_eq!(conns(&server), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_is_not_followed_and_a_big_body_is_refused() {
    let other = serve(|_| ok("never reached")).await;
    let target = format!("http://svc.test:{}/steal", other.port);
    let main = serve(move |_| {
        format!("HTTP/1.1 302 Found\r\nlocation: {target}\r\ncontent-length: 0\r\n\r\n")
    })
    .await;
    let s = site("acme", main.port, "").await;
    let outcome = run(
        &s.handler,
        &http(&format!("http://svc.test:{}/x", main.port)),
    );
    assert!(!is_done(&outcome), "{outcome:?}");
    assert_eq!(
        (conns(&main), conns(&other)),
        (1, 0),
        "only the first hop was dialed"
    );

    let big = serve(|_| ok(&"x".repeat(4096))).await;
    let s = site("acme", big.port, "max_response_bytes = 64").await;
    let outcome = run(
        &s.handler,
        &http(&format!("http://svc.test:{}/x", big.port)),
    );
    assert!(!is_done(&outcome), "{outcome:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_hosted_sites_do_not_share_egress_allowances() {
    let server = serve(|_| ok("{}")).await;
    let a = site("site-a", server.port, "").await;
    let b = bare_site("site-b", resolver_for("svc.test", "127.0.0.1")).await;
    let config = http(&format!("http://svc.test:{}/x", server.port));
    assert!(is_done(&run(&a.handler, &config)));
    assert_eq!(
        code_of(&run(&b.handler, &config)),
        "bpm_integration.egress_denied"
    );
    assert_eq!(conns(&server), 1);
    // Nor a job store: a mail of site A is never seen by site B.
    run(&a.handler, &node("mail", &mail_endpoint()));
    assert_eq!((a.submit.keys().len(), b.submit.keys().len()), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_never_answers_times_out_and_the_site_caps_retries() {
    let hung = serve_with(|_| None).await;
    let s = site("acme", hung.port, "timeout_secs = 1").await;
    let config = http(&format!("http://svc.test:{}/x", hung.port));
    let started = std::time::Instant::now();
    assert_eq!(code_of(&run_node_for(&s.handler, &config, "c", 1)), "retry");
    // The last attempt gives up. The egress sender reports a total timeout as its one transport
    // code (`egress.resolution_failed`), so the node's code is `transport_failed`, not `timeout`.
    let last = run_node_for(&s.handler, &config, "c", 3);
    assert_eq!(
        code_of(&last),
        "bpm_integration.transport_failed",
        "{last:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "bounded by the site timeout"
    );

    // A refused connection is retried; the node asks for ten attempts.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = json!({
        "url": format!("http://svc.test:{port}/x"),
        "retry": { "max_attempts": 10, "backoff_secs": 1 },
    });
    let config = node("http", &endpoint);
    let capped = site("acme", port, "max_retry_attempts = 2").await;
    assert_eq!(
        code_of(&run_node_for(&capped.handler, &config, "c", 1)),
        "retry"
    );
    let second = run_node_for(&capped.handler, &config, "c", 2);
    assert!(
        matches!(second, NodeOutcome::Fail { .. }),
        "capped at the site's two: {second:?}"
    );
    // Without that cap (the default five) attempt 2 still retries, and attempt 5 is held.
    let open = site("acme", port, "").await;
    assert_eq!(
        code_of(&run_node_for(&open.handler, &config, "c", 2)),
        "retry"
    );
    let fifth = run_node_for(&open.handler, &config, "c", 5);
    assert!(matches!(fifth, NodeOutcome::Fail { .. }), "{fifth:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_sealed_credential_fails_the_node_closed_before_any_request() {
    use spot_secrets::{MemoryKeyring, SecretBox, SecretString};

    let keyring = Arc::new(MemoryKeyring::new("test").unwrap());
    let aad = "integration:sales_api";
    let secret_box = SecretBox::new(Arc::clone(&keyring));
    let sealed = secret_box
        .seal(&SecretString::new(CANARY), aad.as_bytes())
        .await
        .unwrap();
    let sealed = sealed.to_string();
    let server = serve(|_| ok("{}")).await;
    let handler = |entries: Vec<(&str, &str, &str)>| {
        let entries = entries
            .into_iter()
            .map(|(n, s, a)| (n.to_owned(), s.to_owned(), a.to_owned()));
        let resolver = sealed_resolver(entries, Arc::clone(&keyring));
        EngineIntegrationHandler::new(http_registry("acme", server.port), resolver)
    };
    let endpoint = json!({
        "url": format!("http://svc.test:{}/x", server.port),
        "auth": { "credential_ref": "sales_api", "scheme": "bearer" },
    });
    let config = node("http", &endpoint);

    // Control: the right sealed value under the right associated data is carried to the server.
    let good = run(&handler(vec![("sales_api", &sealed, aad)]), &config);
    assert!(is_done(&good), "{good:?}");
    assert!(server.heads.lock().unwrap()[0].contains(&format!("Bearer {CANARY}")));
    assert!(!format!("{good:?}").contains(CANARY));

    // Wrong associated data, garbage that is not a sealed value, an unknown name: each fails
    // closed, never retried, with nothing sent and no value in the outcome.
    let bad = "bpm_integration.secret_resolve_failed";
    let cases = [
        (
            vec![("sales_api", sealed.as_str(), "integration:other")],
            bad,
        ),
        (vec![("sales_api", "spsec:v1:not-a-sealed-value", aad)], bad),
        (vec![("sales_api", "plain text", aad)], bad),
        (vec![], "bpm_integration.credential_missing"),
    ];
    for (entries, expected) in cases {
        let outcome = run(&handler(entries), &config);
        assert!(matches!(outcome, NodeOutcome::Fail { .. }), "{outcome:?}");
        assert_eq!(code_of(&outcome), expected);
        assert!(!format!("{outcome:?}").contains(CANARY));
    }
    assert_eq!(
        conns(&server),
        1,
        "no request was made without the credential"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_webhook_is_signed_and_posted_without_its_key_on_the_wire() {
    let server = serve(|_| ok(r#"{"delivered":true}"#)).await;
    let mut secrets = InMemorySecretResolver::new();
    secrets.insert("hook_key", CANARY);
    let handler =
        EngineIntegrationHandler::new(http_registry("acme", server.port), Arc::new(secrets));
    let endpoint = json!({
        "url": format!("http://svc.test:{}/hooks", server.port),
        "credential_ref": "hook_key",
    });
    let outcome = run(&handler, &node("webhook", &endpoint));
    assert!(is_done(&outcome), "{outcome:?}");
    let head = server.heads.lock().unwrap()[0].clone();
    assert!(head.to_lowercase().contains("x-spot-signature"), "{head}");
    assert!(
        !head.contains(CANARY),
        "the signing secret was sent on the wire"
    );
}

fn mail_endpoint() -> Json {
    json!({
        "from": "orders@acme.test",
        "to": ["jane@example.test"],
        "subject": "order confirmed",
        "text": "same words every time",
    })
}

fn message() -> spot_mail::OutgoingMessage {
    let from = spot_mail::Mailbox::new(None, "orders@acme.test").unwrap();
    let mut message = spot_mail::OutgoingMessage::new(from, "hello".to_owned());
    message.to = vec![spot_mail::Mailbox::new(None, "jane@example.test").unwrap()];
    message.text = Some("hi".to_owned());
    message
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mail_node_is_idempotent_per_case_node_and_attempt_not_per_content() {
    let s = bare_site("acme", Arc::new(TestResolver::new())).await;
    let config = node("mail", &mail_endpoint());
    // The same case, node and attempt run twice (a replay after a crash): one job.
    for _ in 0..2 {
        assert!(is_done(&run_node_for(&s.handler, &config, "case_a", 1)));
    }
    assert_eq!(s.submit.submissions.load(Ordering::SeqCst), 2);
    assert_eq!(s.submit.keys(), ["bpm_integration_mail:case_a:n1:1"]);
    // Another case with identical content, and a later attempt: a job of their own each.
    assert!(is_done(&run_node_for(&s.handler, &config, "case_b", 1)));
    assert!(is_done(&run_node_for(&s.handler, &config, "case_a", 2)));
    let keys = s.submit.keys();
    assert_eq!(
        keys[1..],
        [
            "bpm_integration_mail:case_b:n1:1",
            "bpm_integration_mail:case_a:n1:2"
        ]
    );
    let jobs = s.submit.jobs.lock().unwrap();
    assert!(
        jobs.iter()
            .all(|j| j.name == "bpm_integration:send_mail" && j.queue == "default")
    );
}

#[tokio::test]
async fn queued_mail_without_an_engine_key_fails_closed() {
    let submit = Arc::new(RecordingSubmit::default());
    let port = QueueMailPort::new(submit.clone() as Arc<dyn JobSubmit>);
    let error = port.send(&message()).await.unwrap_err();
    assert_eq!(error.code(), "bpm_integration.mail_permanent");
    assert_eq!(submit.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_smtp_host_does_not_fail_the_site_start_and_its_send_retries() {
    let smtp = "sink = \"smtp\"\n[smtp]\nhost = \"smtp.unresolvable.test\"\nport = 587\n\
                security = \"starttls\"\nfrom = \"orders@acme.test\"\n";
    let mail: Mail = toml::from_str(smtp).unwrap();
    let resolver = Arc::new(TestResolver::new());
    let mut s = site_with("acme", None, resolver.clone(), "", &mail).await;
    assert_eq!(
        resolver.call_count("smtp.unresolvable.test"),
        0,
        "start resolves nothing"
    );
    assert!(has_job(&mut s.ctx));
    // The node still queues durably; the job is what retries.
    assert!(is_done(&run(&s.handler, &node("mail", &mail_endpoint()))));

    let mut policy = integration_egress_policy();
    policy.default = "deny_non_public".to_owned();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let guard = Arc::new(Guard::new(
        policy,
        resolver.clone(),
        Arc::new(InMemorySink::new()),
        clock,
    ));
    let relay = SmtpMailPort::new("smtp.unresolvable.test", 587, guard, "acme");
    let port: Arc<dyn spot_bpm_integration::MailPort> = Arc::new(relay);
    let error = port.send(&message()).await.unwrap_err();
    assert_eq!(error.code(), "bpm_integration.transport_failed");
    assert_eq!(error.retry_class(), RetryClass::Transient);
    let args = json!({
        "from": { "name": null, "address": "orders@acme.test" },
        "to": [{ "name": null, "address": "jane@example.test" }],
        "cc": [], "subject": "hello", "text": "hi", "html": null,
    });
    let result = run_send_mail_job(&port, &args).await;
    assert!(
        matches!(result, spot_jobs::worker::HandlerResult::Retry(_)),
        "{result:?}"
    );
    assert!(
        resolver.call_count("smtp.unresolvable.test") >= 2,
        "a failure is not remembered"
    );
}

#[test]
fn the_module_constructs_no_http_client_of_its_own() {
    let source = include_str!("../src/services/process_integration.rs");
    let code: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    let code = code.join("\n");
    let forbidden = [
        "reqwest",
        "hyper::client",
        "hyper_util::client",
        "TcpStream",
        "HttpConnector",
        "UdpSocket",
    ];
    for needle in forbidden {
        assert!(
            !code.contains(needle),
            "the module opens its own client or socket: `{needle}`"
        );
    }
    assert!(code.contains("spot_egress::HttpSender") && code.contains("EgressConnector::new"));
    let manifest = include_str!("../Cargo.toml");
    let added = manifest
        .split("# Integration node hosting (WP-BPM-06.T3)")
        .nth(1);
    let added = added.and_then(|rest| rest.split("# Document node hosting").next());
    for client in ["reqwest", "hyper", "ureq", "isahc"] {
        assert!(
            !added.unwrap_or("").contains(client),
            "this task adds no client: {client}"
        );
    }
}

/// A handler with an HTTP node hosted, so only the runtime can stop the call.
fn bridge() -> EngineIntegrationHandler {
    let mut registry = IntegrationRegistry::new();
    let port = Arc::new(spot_bpm_integration::FakeEgress::new());
    registry.register(HttpNodeHandler::new(port, "acme"));
    EngineIntegrationHandler::new(registry, Arc::new(InMemorySecretResolver::new()))
}

#[test]
fn outside_any_runtime_the_node_fails_closed_instead_of_panicking() {
    let outcome = run(&bridge(), &http("https://api.example.test/x"));
    assert_eq!(code_of(&outcome), "bpm_integration.transport_failed");
}

#[tokio::test(flavor = "current_thread")]
async fn on_a_current_thread_runtime_the_node_fails_closed_instead_of_panicking() {
    let outcome = run(&bridge(), &http("https://api.example.test/x"));
    assert_eq!(code_of(&outcome), "bpm_integration.transport_failed");
}

mod captured {
    use std::fmt::Write as _;
    use std::sync::{Mutex, Once};

    static LOGS: Mutex<String> = Mutex::new(String::new());
    static INIT: Once = Once::new();

    struct Fields<'a>(&'a mut String);
    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    struct Capture;
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut line = event.metadata().target().to_owned();
            event.record(&mut Fields(&mut line));
            if let Ok(mut logs) = LOGS.lock() {
                logs.push_str(&line);
                logs.push('\n');
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Installs the capturing subscriber for the whole test binary (once).
    pub fn install() {
        INIT.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Capture);
        });
    }

    pub fn text() -> String {
        LOGS.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

const PROCESS: &str = r#"
name = "int_host_test"
app = "process_integration_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "call"
kind = "integration"
connector = "http"
action = "post"
endpoint = '__ENDPOINT__'
error = "failed"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[nodes]]
id = "failed"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "call"

[[edges]]
from = "call"
to = "done"
"#;

struct NullSubmit;
impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_credential_never_lands_in_a_stored_run_or_a_log_line() {
    use spot_bpm_definitions::DefinitionStore as _;
    use spot_bpm_defs_pg::PgDefinitionStore;
    use spot_bpm_engine::{HandlerRegistry, InMemoryTimerPort};
    use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
    use spot_cli::services::process::ProcessHost;
    use spot_postgres::PgBackend;
    use spot_postgres::testing::TestDb;
    use spot_scheduler::{InMemoryLeaseStore, LeaseStore};
    use spot_secrets::{MemoryKeyring, SecretBox, SecretString};

    captured::install();
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    // The upstream is hostile: it echoes the Authorization header back in its response body.
    let server = serve(|request| {
        let echoed = request
            .lines()
            .find(|l| l.to_lowercase().starts_with("authorization:"))
            .unwrap_or("");
        ok(&json!({ "echo": echoed, "id": "X-1" }).to_string())
    })
    .await;

    let keyring = Arc::new(MemoryKeyring::new("test").unwrap());
    let aad = "integration:sales_api";
    let secret_box = SecretBox::new(Arc::clone(&keyring));
    let sealed = secret_box
        .seal(&SecretString::new(CANARY), aad.as_bytes())
        .await
        .unwrap();
    let entries = vec![("sales_api".to_owned(), sealed.to_string(), aad.to_owned())];
    let handler = EngineIntegrationHandler::new(
        http_registry("tenant-int", server.port),
        sealed_resolver(entries, keyring),
    );

    let pool = || {
        let mut cfg = db.config();
        cfg.pool_max_size = 4;
        PgBackend::connect_unchecked(cfg).unwrap().pool()
    };
    let store = PgCaseStore::new(pool()).with_tenant("tenant-int");
    store.prepare().await.unwrap();
    let definitions = Arc::new(PgDefinitionStore::new(pool()).with_tenant("tenant-int"));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let at = jiff::Timestamp::new(1_700_000_000, 0).unwrap();
    let lease = leases
        .acquire("timer:tenant-int", "node-1", Duration::from_secs(3600), at)
        .unwrap();
    let timers = FencedTimerPort::new(leases, lease, Arc::new(InMemoryTimerPort::new()));
    let mut handlers = HandlerRegistry::logic_default();
    handlers.register_arc(Arc::new(handler));
    let host = ProcessHost::build(
        SiteId::new("tenant-int").unwrap(),
        store,
        timers,
        handlers,
        definitions.clone(),
        Arc::new(NullSubmit),
        Arc::new(SystemClock::new()),
    );

    let endpoint = json!({
        "url": format!("http://svc.test:{}/orders", server.port),
        "auth": { "credential_ref": "sales_api", "scheme": "bearer" },
        "response": { "remote_id": "doc.response.id", "echoed": "doc.response.echo" },
    });
    let text = PROCESS.replace("__ENDPOINT__", &endpoint.to_string());
    let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    definitions
        .publish("int_host_test", &text, None, "tester", None, now)
        .await
        .unwrap();
    let version = spot_bpm_model::from_text(&text).unwrap();
    host.engine().unwrap().publish(&version).unwrap();
    let start = host.start_run("int_host_test", None, json!({}), None, "order-1", now);
    let case = start.await.unwrap();
    let engine = host.engine().unwrap();
    let record = engine.run_until_idle(&case, now, 20).await.unwrap();

    assert_eq!(record.run.status, spot_dag_exec::RunStatus::Completed);
    let wire = server.heads.lock().unwrap()[0].clone();
    assert!(
        wire.contains(&format!("Bearer {CANARY}")),
        "the call carried the credential"
    );
    let stored = engine.load(&case).await.unwrap();
    let rows = [
        serde_json::to_string(&stored).unwrap(),
        serde_json::to_string(&record).unwrap(),
    ];
    for row in rows {
        assert!(
            !row.contains(CANARY),
            "the secret reached a stored Run row: {row}"
        );
    }
    let logs = captured::text();
    assert!(!logs.is_empty(), "the capture saw log lines");
    assert!(!logs.contains(CANARY), "the secret reached a log line");
}
