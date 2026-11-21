//! WP-SEC-09.T11: the mail and process-integration services use the site's shared egress guard.
//! A policy that denies the host refuses the SMTP check and an integration HTTP call, and the
//! denial is recorded by the shared guard's sink; an allow lets the check through. DNS is
//! scripted and nothing touches the network.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_bpm_engine::{CaseId, IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use spot_cli::config;
use spot_cli::services::SiteCtx;
use spot_cli::services::mail::{Mail, Settings};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::HandlerBuilder;
use spot_cli::services::process_integration::{Wiring, register_with};
use spot_core::{Clock, SiteId, SystemClock};
use spot_dag::NodeId;
use spot_egress::{DenialSink, EgressPolicy, Guard, InMemorySink, Resolver};
use spot_files::memory::MemoryFileStore;

struct StaticResolver(HashMap<String, Vec<IpAddr>>);

#[async_trait::async_trait]
impl Resolver for StaticResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        self.0
            .get(host)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such host"))
    }
}

fn world() -> Arc<StaticResolver> {
    let ip: IpAddr = "93.184.216.34".parse().unwrap();
    Arc::new(StaticResolver(
        ["smtp.acme.test", "api.acme.test"]
            .iter()
            .map(|h| ((*h).to_owned(), vec![ip]))
            .collect(),
    ))
}

/// A shared guard over `policy_toml`, with its own recording sink.
fn shared(policy_toml: &str) -> (Arc<Guard>, Arc<InMemorySink>) {
    let sink = Arc::new(InMemorySink::new());
    let guard = Arc::new(Guard::new(
        EgressPolicy::from_toml(policy_toml).unwrap(),
        world(),
        sink.clone() as Arc<dyn DenialSink>,
        Arc::new(SystemClock::new()),
    ));
    (guard, sink)
}

const DENY_ALL: &str = "[tenants.acme]\n";

const ALLOW_SMTP_AND_API: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "smtp.acme.test"
ports = [587]
purpose = "mail"
[[tenants.acme.allow]]
scheme = "https"
host = "api.acme.test"
ports = [443]
purpose = "connector"
"#;

fn smtp_settings() -> config::Mail {
    config::Mail {
        enabled: true,
        sink: "smtp".to_owned(),
        smtp: Some(config::Smtp {
            host: "smtp.acme.test".to_owned(),
            port: 587,
            security: "starttls".to_owned(),
            from: "noreply@acme.test".to_owned(),
            username: None,
            password: None,
        }),
        ..config::Mail::default()
    }
}

fn mail_settings() -> Settings {
    Settings {
        site: SiteId::new("acme").unwrap(),
        mail: smtp_settings(),
        files: Arc::new(MemoryFileStore::new()),
        sent_log: None,
        clock: Arc::new(SystemClock::new()),
        bounces: None,
    }
}

// --- mail ------------------------------------------------------------------------------------

#[tokio::test]
async fn mail_uses_the_shared_guard_and_a_denied_smtp_host_is_refused_and_recorded() {
    let (guard, sink) = shared(DENY_ALL);
    let mail = Mail::with_guard(mail_settings(), guard);
    assert!(mail.uses_shared_guard());
    let refusal = mail.service().check().await.unwrap_err();
    assert!(refusal.contains("egress.denied_by_policy"), "{refusal}");
    // The shared guard's sink saw it: one denial, for the mail purpose.
    let records = sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].purpose.as_str(), "mail");
    assert_eq!(records[0].tenant, "acme");
}

#[tokio::test]
async fn mail_with_an_allow_entry_in_the_site_policy_reaches_the_host_check() {
    let (guard, sink) = shared(ALLOW_SMTP_AND_API);
    let mail = Mail::with_guard(mail_settings(), guard);
    mail.service().check().await.unwrap();
    assert!(sink.records().is_empty());
}

#[tokio::test]
async fn mail_without_a_shared_guard_keeps_its_private_policy() {
    let mail = Mail::new(Settings {
        mail: config::Mail::default(),
        ..mail_settings()
    });
    assert!(!mail.uses_shared_guard());
    // The memory sink never dials, so it keeps its private guard even when a shared one is given.
    let (guard, _sink) = shared(DENY_ALL);
    let memory = Mail::with_guard(
        Settings {
            mail: config::Mail::default(),
            ..mail_settings()
        },
        guard,
    );
    assert!(!memory.uses_shared_guard());
}

// --- process integration ---------------------------------------------------------------------

#[derive(Default)]
struct RecordingSubmit {
    submissions: AtomicUsize,
    jobs: Mutex<Vec<JobSpec>>,
}

impl JobSubmit for RecordingSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            self.submissions.fetch_add(1, Ordering::SeqCst);
            self.jobs.lock().unwrap().push(job);
            Ok(true)
        })
    }
}

async fn integration_with_shared_guard(
    guard: Arc<Guard>,
) -> Arc<spot_cli::services::process_integration::EngineIntegrationHandler> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock, BTreeMap::new(), true);
    ctx.provide(Arc::new(HandlerBuilder::new())).unwrap();
    ctx.provide::<dyn JobSubmit>(Arc::new(RecordingSubmit::default()) as Arc<dyn JobSubmit>)
        .unwrap();
    // What `services::egress` provides at ORDER 950.
    ctx.provide(guard).unwrap();
    let site = SiteId::new("acme").unwrap();
    // The wiring's own policy would allow any public host: the shared guard must win.
    register_with(
        &mut ctx,
        &site,
        &config::Mail::default(),
        Wiring::production(),
    )
    .await
    .unwrap()
    .expect("an enabled module registers a handler")
}

fn run_http(
    handler: &spot_cli::services::process_integration::EngineIntegrationHandler,
    url: &str,
) -> NodeOutcome {
    let case = CaseId::new("case1").unwrap();
    let node = NodeId::new("n1").unwrap();
    let key = IdempotencyKey::new(case.clone(), node.clone(), 1);
    let mut variables = json!({});
    let config: Json = json!({
        "connector": "http",
        "action": "x",
        "endpoint": json!({ "url": url }).to_string(),
    });
    let mut ctx = NodeCtx {
        case: &case,
        node: &node,
        kind: NodeKind::Integration,
        attempt: 1,
        key,
        is_retry: false,
        now: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        deadline: None,
        principal: None,
        config: &config,
        outgoing: &[],
        variables: &mut variables,
        cancelled: false,
    };
    handler.run(&mut ctx)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_integration_http_call_the_site_policy_denies_is_refused_and_recorded() {
    let (guard, sink) = shared(DENY_ALL);
    let handler = integration_with_shared_guard(guard).await;
    let url = "https://api.acme.test/orders";
    let outcome = run_http(&handler, url);
    match outcome {
        NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.egress_denied"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let records = sink.records();
    assert_eq!(records.len(), 1, "the denial is on the shared guard's sink");
    assert_eq!(records[0].purpose.as_str(), "connector");
    assert_eq!(records[0].host, "api.acme.test");
}
