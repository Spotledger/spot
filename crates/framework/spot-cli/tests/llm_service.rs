//! WP-AI-03.T5: the LLM service. The gateway runs over an in-memory store, a real egress guard
//! (a scripted resolver, so no DNS) and a scripted egress sender that counts every request it
//! receives: a refusal that must send nothing is proved by the sender's count staying at zero.
//! The one PostgreSQL test needs `SPOT_TEST_PG_URL` and skips without it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderMap, StatusCode};
use spot_cli::services::egress::SharedGuard;
use spot_cli::services::llm::config::{BudgetConfig, Kind, Period, PriceConfig, TenantConfig};
use spot_cli::services::llm::gateway::{
    BuiltinProviders, CallError, Caller, Deps, LlmGateway, ProviderFactory, TransportFactory,
};
use spot_cli::services::llm::prompt::{
    DocumentReader, DocumentView, FieldClass, FieldValue, PromptPolicy, ReadRefusal,
};
use spot_cli::services::llm::store::{SendRecord, Store, UsageRow};
use spot_core::ManualClock;
use spot_document::secrets::SecretVault;
use spot_egress::{
    Authorized, EgressError, EgressPolicy, Guard, HttpRequest, InMemorySink, Purpose, Resolver,
    Response as WireResponse, Sender,
};
use spot_llm::transport::egress::EgressTransport;
use spot_llm::transport::mock::{MockTransport, RecordedExchange, RecordedResponse};
use spot_llm::transport::{HttpRequest as LlmHttpRequest, HttpTransport};
use spot_llm::{
    ApiKey, BoxFuture, LlmError, Message, MockAction, MockProvider, MockStep, Provider, Response,
    StopReason, Usage,
};
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::{MemoryKeyring, SecretBox, SecretString};
use time::OffsetDateTime;

const KEY_ACME: &str = "sk-test-ACME-0123456789";
const KEY_GLOBEX: &str = "sk-test-GLOBEX-9876543210";

const POLICY: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "api.anthropic.test"
purpose = "llm"

[tenants.globex]
[[tenants.globex.allow]]
host = "api.openrouter.test"
purpose = "llm"

[tenants.rebinder]
[[tenants.rebinder.allow]]
host = "rebind.test"
purpose = "llm"
"#;

const ANTHROPIC_OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"5"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
const OPENAI_OK: &str = r#"{"id":"or-1","choices":[{"index":0,"message":{"role":"assistant","content":"5"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4}}"#;
const ANTHROPIC_AUTH: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;

/// A resolver that always answers the same addresses and counts its lookups.
struct Resolve {
    hosts: HashMap<String, Vec<IpAddr>>,
    lookups: Mutex<HashMap<String, usize>>,
}

impl Resolve {
    fn new() -> Self {
        let mut hosts = HashMap::new();
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        for host in ["api.anthropic.test", "api.openrouter.test", "evil.test"] {
            hosts.insert(host.to_owned(), vec![public]);
        }
        // A name that points inside the network.
        hosts.insert("rebind.test".to_owned(), vec!["10.0.0.5".parse().unwrap()]);
        Self {
            hosts,
            lookups: Mutex::new(HashMap::new()),
        }
    }

    fn lookups_of(&self, host: &str) -> usize {
        self.lookups.lock().unwrap().get(host).copied().unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl Resolver for Resolve {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        *self
            .lookups
            .lock()
            .unwrap()
            .entry(host.to_owned())
            .or_insert(0) += 1;
        self.hosts
            .get(host)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unknown host"))
    }
}

/// What the sender saw of one request.
#[derive(Clone, Debug)]
struct Seen {
    url: String,
    headers: String,
    body: String,
}

/// The scripted egress sender: counts requests, keeps what it saw, answers with a canned body.
struct Stub {
    calls: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
    /// When non-zero, the status every request is answered with (an Anthropic auth error body).
    force_status: AtomicU16,
}

impl Stub {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            force_status: AtomicU16::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Sender for Stub {
    async fn send_request(
        &self,
        request: &HttpRequest,
        _authorized: &Authorized,
        _max_body: u64,
    ) -> Result<WireResponse, EgressError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let headers = request
            .headers
            .iter()
            .map(|(name, value)| format!("{}: {}", name, value.to_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n");
        let body = request
            .body
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let url = request.url.to_string();
        self.seen.lock().unwrap().push(Seen {
            url: url.clone(),
            headers,
            body,
        });
        let forced = self.force_status.load(Ordering::SeqCst);
        let (status, body) = if forced != 0 {
            (
                StatusCode::from_u16(forced).unwrap(),
                ANTHROPIC_AUTH.as_bytes().to_vec(),
            )
        } else if url.contains("/v1/messages") {
            (StatusCode::OK, ANTHROPIC_OK.as_bytes().to_vec())
        } else {
            (StatusCode::OK, OPENAI_OK.as_bytes().to_vec())
        };
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        Ok(WireResponse {
            status,
            headers,
            body,
        })
    }
}

struct ViaStub(Arc<Stub>);

impl TransportFactory for ViaStub {
    fn transport(&self, guard: Arc<Guard>, tenant: &str) -> Arc<dyn HttpTransport> {
        Arc::new(EgressTransport::new(
            guard,
            Arc::clone(&self.0) as Arc<dyn Sender>,
            tenant,
            Purpose::Llm,
        ))
    }
}

/// Replays recorded exchanges: the guard is not on this path (the gateway still authorises first).
struct Recorded(Arc<MockTransport>);

impl TransportFactory for Recorded {
    fn transport(&self, _guard: Arc<Guard>, _tenant: &str) -> Arc<dyn HttpTransport> {
        Arc::clone(&self.0) as Arc<dyn HttpTransport>
    }
}

struct MockFactory(Arc<MockProvider>);

impl ProviderFactory for MockFactory {
    fn build(
        &self,
        _config: &TenantConfig,
        _key: Option<ApiKey>,
        _transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider> {
        Arc::clone(&self.0) as Arc<dyn Provider>
    }
}

struct Reader(DocumentView);

impl DocumentReader for Reader {
    fn read<'a>(
        &'a self,
        _user: &'a str,
        _doctype: &'a str,
        _name: &'a str,
    ) -> BoxFuture<'a, Result<DocumentView, ReadRefusal>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

struct Rig {
    gw: LlmGateway,
    store: Store,
    clock: Arc<ManualClock>,
    stub: Arc<Stub>,
    resolver: Arc<Resolve>,
    /// The site's keyring: a restart keeps it, so the sealed keys stay readable.
    vault: Arc<dyn SecretVault>,
}

fn document() -> DocumentView {
    let field = |name: &str, class, value: &str| FieldValue {
        name: name.to_owned(),
        class,
        value: value.to_owned(),
    };
    DocumentView {
        doctype: "Task".to_owned(),
        name: "T-1".to_owned(),
        fields: vec![
            field("subject", FieldClass::Plain, "fix the printer"),
            field("details", FieldClass::Protected, "PROTECTED-VALUE-XYZ"),
            field("api_secret", FieldClass::Sealed, "SEALED-VALUE-XYZ"),
        ],
    }
}

fn clock() -> Arc<ManualClock> {
    // 2026-11-21 12:00 UTC.
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ))
}

fn rig_over(
    store: Store,
    clock: Arc<ManualClock>,
    providers: Arc<dyn ProviderFactory>,
    transports: Option<Arc<dyn TransportFactory>>,
) -> Rig {
    let vault: Arc<dyn SecretVault> =
        Arc::new(SecretBox::new(MemoryKeyring::new("llm-test").unwrap()));
    rig_with(store, clock, providers, transports, vault)
}

fn rig_with(
    store: Store,
    clock: Arc<ManualClock>,
    providers: Arc<dyn ProviderFactory>,
    transports: Option<Arc<dyn TransportFactory>>,
    vault: Arc<dyn SecretVault>,
) -> Rig {
    let resolver = Arc::new(Resolve::new());
    let guard = Arc::new(Guard::new(
        EgressPolicy::from_toml(POLICY).unwrap(),
        Arc::clone(&resolver) as Arc<dyn Resolver>,
        Arc::new(InMemorySink::new()),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    ));
    let stub = Arc::new(Stub::new());
    let transports = transports.unwrap_or_else(|| Arc::new(ViaStub(Arc::clone(&stub))));
    let gw = LlmGateway::new(Deps {
        site: "site-1".to_owned(),
        store: store.clone(),
        vault: Arc::clone(&vault),
        guard: Arc::new(SharedGuard::new(guard)),
        clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        providers,
        transports,
        redactor: Arc::new(Redactor::from_policy(&RedactionPolicy::default()).unwrap()),
        reader: Arc::new(Reader(document())),
        max_retries: 0,
        retention_days: 1,
    });
    Rig {
        gw,
        store,
        clock,
        stub,
        resolver,
        vault,
    }
}

fn rig() -> Rig {
    rig_over(Store::memory(), clock(), Arc::new(BuiltinProviders), None)
}

fn anthropic(tenant: &str) -> TenantConfig {
    TenantConfig {
        tenant: tenant.to_owned(),
        kind: Kind::Anthropic,
        endpoint: "https://api.anthropic.test".to_owned(),
        model: "claude-test".to_owned(),
        allowed_hosts: vec!["api.anthropic.test".to_owned()],
        // One micro-unit per token, both ways.
        price: PriceConfig {
            input_micro_per_million: 1_000_000,
            output_micro_per_million: 1_000_000,
        },
        context_window: 100_000,
        max_output: 4_096,
        default_max_output_tokens: 100,
        tasks: vec!["chat".to_owned()],
        budget: None,
    }
}

fn openrouter(tenant: &str) -> TenantConfig {
    TenantConfig {
        kind: Kind::Openrouter,
        endpoint: "https://api.openrouter.test".to_owned(),
        model: "meta/llama-test".to_owned(),
        allowed_hosts: vec!["api.openrouter.test".to_owned()],
        ..anthropic(tenant)
    }
}

fn caller() -> Caller {
    Caller {
        user: "user-1".to_owned(),
        purpose: "unit test".to_owned(),
    }
}

async fn ask(rig: &Rig, tenant: &str, task: &str, id: &str) -> Result<Response, CallError> {
    let (request, manifest) = rig
        .gw
        .prompt("user-1", PromptPolicy::default())
        .system("be brief")
        .text("Add 2 and 3.")
        .build("ignored", id);
    rig.gw
        .complete(tenant, task, &caller(), request, &manifest)
        .await
}

async fn ready(rig: &Rig, config: &TenantConfig, key: &str) {
    rig.gw.put_config(config).await.unwrap();
    rig.gw
        .set_key(&config.tenant, &SecretString::new(key))
        .await
        .unwrap();
}

/// The scripted agent: one question, one answer. It runs unchanged on every provider.
async fn agent(rig: &Rig, tenant: &str) -> String {
    ask(rig, tenant, "chat", "agent-1").await.unwrap().text()
}

#[tokio::test]
async fn the_same_agent_runs_on_every_provider() {
    // Anthropic and an OpenAI-compatible endpoint, through the egress transport.
    let r = rig();
    ready(&r, &anthropic("acme"), KEY_ACME).await;
    ready(&r, &openrouter("globex"), KEY_GLOBEX).await;
    assert_eq!(agent(&r, "acme").await, "5");
    assert_eq!(agent(&r, "globex").await, "5");
    assert_eq!(r.stub.calls(), 2);

    // The mock provider.
    let mut mock = MockProvider::new();
    mock.push_step(MockStep::new(
        None,
        MockAction::Respond(Response::new(
            Message::assistant("5"),
            StopReason::Stop,
            Usage {
                input_tokens: 12,
                output_tokens: 4,
                ..Usage::ZERO
            },
        )),
    ));
    let m = rig_over(
        Store::memory(),
        clock(),
        Arc::new(MockFactory(Arc::new(mock))),
        None,
    );
    ready(&m, &anthropic("acme"), KEY_ACME).await;
    assert_eq!(agent(&m, "acme").await, "5");

    // Recorded exchanges for both wire protocols.
    let exchange = |url: &str, body: &str| RecordedExchange {
        request: LlmHttpRequest::post_json(url, ""),
        response: RecordedResponse::Complete {
            status: 200,
            headers: Vec::new(),
            body: body.to_owned(),
        },
    };
    let recorded = Arc::new(MockTransport::new(vec![
        exchange("https://api.anthropic.test/v1/messages", ANTHROPIC_OK),
        exchange("https://api.openrouter.test/v1/chat/completions", OPENAI_OK),
    ]));
    let c = rig_over(
        Store::memory(),
        clock(),
        Arc::new(BuiltinProviders),
        Some(Arc::new(Recorded(recorded))),
    );
    ready(&c, &anthropic("acme"), KEY_ACME).await;
    ready(&c, &openrouter("globex"), KEY_GLOBEX).await;
    assert_eq!(agent(&c, "acme").await, "5");
    assert_eq!(agent(&c, "globex").await, "5");
    assert_eq!(
        c.stub.calls(),
        0,
        "recorded exchanges never reach the sender"
    );
}

#[tokio::test]
async fn the_budget_stops_the_sequence_before_the_provider_is_called() {
    let r = rig();
    let mut config = anthropic("acme");
    // Each call settles at 16 micro-units (12 in, 4 out); the worst case reserved is the input
    // estimate (5) plus the default output cap (100).
    config.budget = Some(BudgetConfig {
        limit_micro: 130,
        period: Period::Day,
        warn_at_percent: 0,
    });
    ready(&r, &config, KEY_ACME).await;
    assert!(ask(&r, "acme", "chat", "c1").await.is_ok());
    assert!(ask(&r, "acme", "chat", "c2").await.is_ok());
    assert_eq!(r.stub.calls(), 2);
    let refused = ask(&r, "acme", "chat", "c3").await.unwrap_err();
    assert!(
        matches!(refused, CallError::BudgetExceeded { .. }),
        "{refused}"
    );
    assert_eq!(refused.code(), "llm_host.budget_exceeded");
    assert_eq!(r.stub.calls(), 2, "the third call sent nothing");

    // The record says it was refused.
    let sends = r.gw.sends("acme", 10).await.unwrap();
    assert_eq!(sends.len(), 3);
    assert!(sends.iter().any(|s| s.outcome == "refused_budget"));
    assert_eq!(sends.iter().filter(|s| s.outcome == "sent").count(), 2);

    // Usage is durable and exact.
    let usage = r.gw.usage("acme").await.unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(
        (usage[0].requests, usage[0].input, usage[0].output),
        (2, 24, 8)
    );
    assert_eq!(usage[0].cost_micro, 32);

    // A new gateway over the same store (a restart) still refuses: the spend is read back.
    let again = rig_with(
        r.store.clone(),
        Arc::clone(&r.clock),
        Arc::new(BuiltinProviders),
        None,
        Arc::clone(&r.vault),
    );
    let refused = ask(&again, "acme", "chat", "c4").await.unwrap_err();
    assert!(matches!(refused, CallError::BudgetExceeded { .. }));
    assert_eq!(again.stub.calls(), 0);

    // The next day the period starts again.
    r.clock.advance(Duration::from_secs(86_400));
    assert!(ask(&r, "acme", "chat", "c5").await.is_ok());
    assert_eq!(r.stub.calls(), 3);
}

#[tokio::test]
async fn an_endpoint_off_the_allow_list_is_refused_and_nothing_is_sent() {
    let r = rig();
    // The gateway will not store a configuration whose endpoint its own list does not cover.
    let mut bad = anthropic("acme");
    bad.endpoint = "https://evil.test".to_owned();
    assert!(matches!(
        r.gw.put_config(&bad).await,
        Err(CallError::Config(_))
    ));

    // A row that got there another way (an older version, a manual edit) is refused at call time.
    r.store.put_config(&bad, 1).await.unwrap();
    r.gw.set_key("acme", &SecretString::new(KEY_ACME))
        .await
        .unwrap();
    let refused = ask(&r, "acme", "chat", "x1").await.unwrap_err();
    assert_eq!(refused, CallError::EndpointNotAllowed);
    assert_eq!(r.stub.calls(), 0);
    assert_eq!(r.resolver.lookups_of("evil.test"), 0, "not even a lookup");
    let sends = r.gw.sends("acme", 5).await.unwrap();
    assert_eq!(sends[0].outcome, "refused_allow_list");
}

#[tokio::test]
async fn the_egress_policy_refuses_a_tenant_it_does_not_list() {
    let r = rig();
    // `umbrella` allows the host itself, but the operator's egress policy has no entry for it.
    ready(&r, &anthropic("umbrella"), KEY_ACME).await;
    let refused = ask(&r, "umbrella", "chat", "x1").await.unwrap_err();
    assert!(matches!(refused, CallError::EndpointDenied(_)), "{refused}");
    assert_eq!(r.stub.calls(), 0);
}

#[tokio::test]
async fn an_endpoint_that_resolves_to_a_private_address_is_refused() {
    let r = rig();
    let mut config = anthropic("rebinder");
    config.endpoint = "https://rebind.test".to_owned();
    config.allowed_hosts = vec!["rebind.test".to_owned()];
    ready(&r, &config, KEY_ACME).await;
    let refused = ask(&r, "rebinder", "chat", "x1").await.unwrap_err();
    assert_eq!(refused, CallError::EndpointDenied("egress.denied_address"));
    assert_eq!(r.stub.calls(), 0);
    assert!(r.resolver.lookups_of("rebind.test") >= 1);
}

#[tokio::test]
async fn a_protected_value_never_appears_in_an_outgoing_request() {
    let r = rig();
    ready(&r, &anthropic("acme"), KEY_ACME).await;
    let (request, manifest) =
        r.gw.prompt("user-1", PromptPolicy::default())
            .text("Summarise this task.")
            .document("Task", "T-1")
            .await
            .unwrap()
            .build("x", "doc-1");
    r.gw.complete("acme", "chat", &caller(), request, &manifest)
        .await
        .unwrap();
    let seen = r.stub.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].body.contains("fix the printer"));
    for hidden in ["PROTECTED-VALUE-XYZ", "SEALED-VALUE-XYZ"] {
        assert!(!seen[0].body.contains(hidden), "{hidden} was sent");
    }

    // The record names classes and DocTypes only.
    let record = &r.gw.sends("acme", 1).await.unwrap()[0];
    assert_eq!(record.doctypes, vec!["Task".to_owned()]);
    assert_eq!(record.classes.get("plain"), Some(&1));
    assert_eq!(record.classes.get("stripped_protected"), Some(&1));
    assert_eq!(record.classes.get("stripped_sealed"), Some(&1));
    assert_eq!(record.endpoint, "https://api.anthropic.test");
    assert_eq!(record.digest.len(), 64);
    assert!(record.digest.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(record.size_bytes > 0);
    assert_eq!(record.principal, "user-1");
    assert_eq!(record.purpose, "unit test");
    assert_eq!(record.outcome, "sent");
    assert_eq!((record.input_tokens, record.output_tokens), (12, 4));
    let shown = format!("{record:?}");
    for content in [
        "fix the printer",
        "Summarise this task",
        "PROTECTED-VALUE-XYZ",
    ] {
        assert!(
            !shown.contains(content),
            "the record holds content: {content}"
        );
    }
}

#[tokio::test]
async fn the_key_is_sealed_and_never_reaches_an_error_a_record_or_the_store() {
    let r = rig();
    ready(&r, &anthropic("acme"), KEY_ACME).await;

    // The sealed column is not the plaintext.
    let sealed = r.store.key("acme").await.unwrap().unwrap();
    assert!(
        !sealed
            .windows(KEY_ACME.len())
            .any(|w| w == KEY_ACME.as_bytes())
    );

    // The key does travel, once, in the provider's own header.
    ask(&r, "acme", "chat", "k1").await.unwrap();
    let seen = r.stub.seen();
    assert_eq!(
        seen.iter().filter(|s| s.headers.contains(KEY_ACME)).count(),
        1
    );
    assert!(!seen[0].body.contains(KEY_ACME));

    // A provider failure: the typed code, and no key anywhere a person could read.
    r.stub.force_status.store(401, Ordering::SeqCst);
    let failed = ask(&r, "acme", "chat", "k2").await.unwrap_err();
    assert_eq!(failed, CallError::Provider(LlmError::Auth));
    let mut shown = format!("{failed} {failed:?}");
    shown.push_str(&format!("{:?}", r.gw.sends("acme", 10).await.unwrap()));
    shown.push_str(&format!("{:?}", r.gw.usage("acme").await.unwrap()));
    shown.push_str(&format!("{:?}", r.gw.config("acme").await.unwrap()));
    assert!(!shown.contains(KEY_ACME));
    let sends = r.gw.sends("acme", 10).await.unwrap();
    assert!(sends.iter().any(|s| s.outcome == "failed_llm_auth"));
    // A refused call (no key stored) also names none.
    r.gw.put_config(&anthropic("nokey")).await.unwrap();
    let refused = ask(&r, "nokey", "chat", "k3").await.unwrap_err();
    assert_eq!(refused, CallError::KeyMissing);
}

#[tokio::test]
async fn two_tenants_have_independent_budgets_and_keys() {
    let r = rig();
    let mut acme = anthropic("acme");
    acme.budget = Some(BudgetConfig {
        limit_micro: 110,
        period: Period::Day,
        warn_at_percent: 0,
    });
    ready(&r, &acme, KEY_ACME).await;
    ready(&r, &openrouter("globex"), KEY_GLOBEX).await;

    assert!(ask(&r, "acme", "chat", "a1").await.is_ok());
    assert!(matches!(
        ask(&r, "acme", "chat", "a2").await,
        Err(CallError::BudgetExceeded { .. })
    ));
    // globex has no budget and is not affected by acme's spend.
    for id in ["g1", "g2", "g3"] {
        assert!(ask(&r, "globex", "chat", id).await.is_ok());
    }
    let seen = r.stub.seen();
    let acme_calls: Vec<_> = seen
        .iter()
        .filter(|s| s.url.contains("anthropic"))
        .collect();
    let globex_calls: Vec<_> = seen
        .iter()
        .filter(|s| s.url.contains("openrouter"))
        .collect();
    assert_eq!(acme_calls.len(), 1);
    assert_eq!(globex_calls.len(), 3);
    assert!(
        acme_calls
            .iter()
            .all(|s| s.headers.contains(KEY_ACME) && !s.headers.contains(KEY_GLOBEX))
    );
    assert!(
        globex_calls
            .iter()
            .all(|s| s.headers.contains(KEY_GLOBEX) && !s.headers.contains(KEY_ACME))
    );
    assert_eq!(r.gw.usage("acme").await.unwrap()[0].requests, 1);
    assert_eq!(r.gw.usage("globex").await.unwrap()[0].requests, 3);
}

#[tokio::test]
async fn a_task_the_tenant_does_not_list_and_a_missing_configuration_are_refused() {
    let r = rig();
    let mut config = anthropic("acme");
    config.tasks = vec!["chat".to_owned(), "summarise".to_owned()];
    ready(&r, &config, KEY_ACME).await;
    assert!(ask(&r, "acme", "chat", "t1").await.is_ok());
    assert!(ask(&r, "acme", "summarise", "t2").await.is_ok());
    assert_eq!(
        ask(&r, "acme", "other", "t3").await.unwrap_err(),
        CallError::TaskUnknown
    );
    assert_eq!(
        ask(&r, "nobody", "chat", "t4").await.unwrap_err(),
        CallError::NotConfigured
    );
    let usage = r.gw.usage("acme").await.unwrap();
    let mut tasks: Vec<&str> = usage.iter().map(|u| u.task.as_str()).collect();
    tasks.sort_unstable();
    assert_eq!(tasks, ["chat", "summarise"]);
    assert_eq!(r.stub.calls(), 2);
}

#[tokio::test]
async fn send_records_are_kept_for_the_retention_period_only() {
    let r = rig();
    ready(&r, &anthropic("acme"), KEY_ACME).await;
    ask(&r, "acme", "chat", "p1").await.unwrap();
    assert_eq!(r.gw.purge_old().await.unwrap(), 0);
    r.clock.advance(Duration::from_secs(2 * 86_400));
    assert_eq!(r.gw.purge_old().await.unwrap(), 1);
    assert!(r.gw.sends("acme", 10).await.unwrap().is_empty());
    // The usage is not a send record: it stays.
    assert_eq!(r.gw.usage("acme").await.unwrap()[0].requests, 1);
}

#[tokio::test]
async fn a_key_needs_a_configuration_and_a_value() {
    let r = rig();
    assert!(
        r.gw.set_key("acme", &SecretString::new(KEY_ACME))
            .await
            .is_err()
    );
    r.gw.put_config(&anthropic("acme")).await.unwrap();
    assert!(r.gw.set_key("acme", &SecretString::new("")).await.is_err());
    assert!(!r.gw.has_key("acme").await.unwrap());
    r.gw.set_key("acme", &SecretString::new(KEY_ACME))
        .await
        .unwrap();
    assert!(r.gw.has_key("acme").await.unwrap());
}

#[tokio::test]
async fn the_postgres_store_round_trips_and_keeps_sites_apart() {
    let Some(db) = spot_postgres::testing::TestDb::create().await else {
        return;
    };
    let backend = spot_postgres::PgBackend::connect(db.config())
        .await
        .unwrap();
    let a = Store::pg(backend.pool(), "site-a");
    let b = Store::pg(backend.pool(), "site-b");
    a.prepare().await.unwrap();
    a.prepare().await.unwrap();

    a.put_config(&anthropic("acme"), 1).await.unwrap();
    a.put_key("acme", b"sealed-bytes", 2).await.unwrap();
    a.put_config(&anthropic("acme"), 3).await.unwrap();
    assert_eq!(
        a.key("acme").await.unwrap().as_deref(),
        Some(&b"sealed-bytes"[..])
    );
    assert_eq!(
        a.config("acme").await.unwrap().unwrap().model,
        "claude-test"
    );
    assert!(b.config("acme").await.unwrap().is_none());
    assert!(b.key("acme").await.unwrap().is_none());

    let row = UsageRow {
        tenant: "acme".to_owned(),
        task: "chat".to_owned(),
        day: 20_413,
        provider: "anthropic".to_owned(),
        model: "claude-test".to_owned(),
        input: 12,
        output: 4,
        requests: 1,
        cost_micro: 16,
        ..UsageRow::default()
    };
    a.add_usage(&row).await.unwrap();
    a.add_usage(&row).await.unwrap();
    let rows = a.usage_since("acme", 0).await.unwrap();
    assert_eq!(
        (rows.len(), rows[0].requests, rows[0].cost_micro),
        (1, 2, 32)
    );
    assert!(b.usage_since("acme", 0).await.unwrap().is_empty());

    let record = SendRecord {
        request_id: "acme:chat:1".to_owned(),
        tenant: "acme".to_owned(),
        task: "chat".to_owned(),
        at_ms: 5,
        endpoint: "https://api.anthropic.test".to_owned(),
        digest: "ab".repeat(32),
        doctypes: vec!["Task".to_owned()],
        outcome: "sending".to_owned(),
        ..SendRecord::default()
    };
    a.put_send(&record).await.unwrap();
    a.put_send(&SendRecord {
        outcome: "sent".to_owned(),
        input_tokens: 12,
        ..record.clone()
    })
    .await
    .unwrap();
    let sends = a.sends("acme", 10).await.unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(
        (sends[0].outcome.as_str(), sends[0].input_tokens),
        ("sent", 12)
    );
    assert_eq!(sends[0].doctypes, vec!["Task".to_owned()]);
    assert!(b.sends("acme", 10).await.unwrap().is_empty());
    assert_eq!(a.purge_sends_before(10).await.unwrap(), 1);
}
