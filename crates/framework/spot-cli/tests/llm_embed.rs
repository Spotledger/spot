//! WP-AI-03.T7: embeddings through the LLM service. The gateway runs over an in-memory store, a
//! real egress guard (a scripted resolver, so no DNS) and either the scripted egress sender
//! (which counts every request it receives: a refusal that must send nothing is proved by the
//! count staying at zero) or the `MockEmbedder` behind a provider factory.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use http::{HeaderMap, StatusCode};
use spot_agent_memory::Embedder as MemoryEmbedder;
use spot_cli::services::egress::SharedGuard;
use spot_cli::services::llm::config::{BudgetConfig, Kind, Period, PriceConfig, TenantConfig};
use spot_cli::services::llm::embed_port::GatewayEmbedPort;
use spot_cli::services::llm::embed_sync::{GatewayEmbedder, check_width};
use spot_cli::services::llm::gateway::{
    BuiltinProviders, CallError, Caller, Deps, EmbedLimits, LlmGateway, ProviderFactory,
    TransportFactory,
};
use spot_cli::services::llm::prompt::{DenyAllReader, PromptPolicy};
use spot_cli::services::llm::store::{SendRecord, Store};
use spot_cli::services::retrieval::ports::{EmbedPort, EmbedPurpose, EmbedRequest as PortRequest};
use spot_core::ManualClock;
use spot_document::secrets::SecretVault;
use spot_egress::{
    Authorized, EgressError, EgressPolicy, Guard, HttpRequest, InMemorySink, Purpose, Resolver,
    Response as WireResponse, Sender,
};
use spot_llm::transport::HttpTransport;
use spot_llm::transport::egress::EgressTransport;
use spot_llm::{ApiKey, Embedder as ProviderEmbedder, MockEmbedder, MockProvider, Provider};
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::{MemoryKeyring, SecretBox, SecretString};
use time::OffsetDateTime;
use tokio::runtime::Handle;

const KEY: &str = "sk-test-EMBED-0123456789";
const SECRET_TEXT: &str = "CONFIDENTIAL-NOTE-123";

const POLICY: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "api.openrouter.test"
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

const OPENAI_OK: &str = r#"{"id":"or-1","choices":[{"index":0,"message":{"role":"assistant","content":"5"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4}}"#;
const AUTH_ERROR: &str = r#"{"error":{"type":"authentication_error","message":"invalid api key"}}"#;

/// A resolver that answers fixed addresses and counts its lookups.
struct Resolve {
    hosts: HashMap<String, Vec<IpAddr>>,
    lookups: Mutex<HashMap<String, usize>>,
}

impl Resolve {
    fn new() -> Self {
        let mut hosts = HashMap::new();
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        for host in ["api.openrouter.test", "evil.test"] {
            hosts.insert(host.to_owned(), vec![public]);
        }
        // A name that points inside the network.
        hosts.insert("rebind.test".to_owned(), vec!["10.0.0.5".parse().unwrap()]);
        Self {
            hosts,
            lookups: Mutex::new(HashMap::new()),
        }
    }

    fn total_lookups(&self) -> usize {
        self.lookups.lock().unwrap().values().sum()
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

/// The scripted egress sender: counts requests, keeps what it saw, answers embeddings with
/// `width`-wide vectors and seven prompt tokens per input, and chat with a canned completion.
struct Stub {
    calls: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
    /// When non-zero, the status every request is answered with (an auth error body).
    force_status: AtomicU16,
    /// The store whose send records are looked at on every request, for a tenant.
    watch: Mutex<Option<(Store, String)>>,
    /// The send records as they were while a request was in flight.
    during: Mutex<Vec<Vec<SendRecord>>>,
    width: usize,
}

impl Stub {
    fn new(width: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            force_status: AtomicU16::new(0),
            watch: Mutex::new(None),
            during: Mutex::new(Vec::new()),
            width,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn watch(&self, store: Store, tenant: &str) {
        *self.watch.lock().unwrap() = Some((store, tenant.to_owned()));
    }
}

/// The reply of an embeddings endpoint to `request_body`.
fn embeddings_body(request_body: &str, width: usize) -> String {
    let parsed: serde_json::Value = serde_json::from_str(request_body).unwrap_or_default();
    let inputs = parsed["input"].as_array().map_or(0, Vec::len);
    let data: Vec<serde_json::Value> = (0..inputs)
        .map(|index| {
            serde_json::json!({
                "object": "embedding",
                "index": index,
                "embedding": vec![0.5f32; width],
            })
        })
        .collect();
    serde_json::json!({
        "object": "list",
        "data": data,
        "model": "embed-test",
        "usage": { "prompt_tokens": inputs * 7, "total_tokens": inputs * 7 },
    })
    .to_string()
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
            body: body.clone(),
        });
        // The records as they are while this request is in flight.
        let watched = self.watch.lock().unwrap().clone();
        if let Some((store, tenant)) = watched {
            let sends = store.sends(&tenant, 100).await.unwrap_or_default();
            self.during.lock().unwrap().push(sends);
        }
        let forced = self.force_status.load(Ordering::SeqCst);
        let (status, reply) = if forced != 0 {
            (StatusCode::from_u16(forced).unwrap(), AUTH_ERROR.to_owned())
        } else if url.contains("/v1/embeddings") {
            (StatusCode::OK, embeddings_body(&body, self.width))
        } else {
            (StatusCode::OK, OPENAI_OK.to_owned())
        };
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        Ok(WireResponse {
            status,
            headers,
            body: reply.into_bytes(),
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

/// Builds a mock chat provider and the shared `MockEmbedder` for every tenant.
struct MockFactory(Arc<MockEmbedder>);

impl ProviderFactory for MockFactory {
    fn build(
        &self,
        _config: &TenantConfig,
        _key: Option<ApiKey>,
        _transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider> {
        Arc::new(MockProvider::new())
    }

    fn build_embedder(
        &self,
        _config: &TenantConfig,
        _key: Option<ApiKey>,
        _transport: Arc<dyn HttpTransport>,
    ) -> Option<Arc<dyn ProviderEmbedder>> {
        Some(Arc::clone(&self.0) as Arc<dyn ProviderEmbedder>)
    }
}

struct Rig {
    gw: Arc<LlmGateway>,
    store: Store,
    stub: Arc<Stub>,
    resolver: Arc<Resolve>,
}

fn clock() -> Arc<ManualClock> {
    // 2026-11-21 12:00 UTC.
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ))
}

fn rig_over(providers: Arc<dyn ProviderFactory>, limits: EmbedLimits) -> Rig {
    let store = Store::memory();
    let clock = clock();
    let vault: Arc<dyn SecretVault> = Arc::new(SecretBox::new(
        MemoryKeyring::new("llm-embed-test").unwrap(),
    ));
    let resolver = Arc::new(Resolve::new());
    let guard = Arc::new(Guard::new(
        EgressPolicy::from_toml(POLICY).unwrap(),
        Arc::clone(&resolver) as Arc<dyn Resolver>,
        Arc::new(InMemorySink::new()),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    ));
    let stub = Arc::new(Stub::new(8));
    let gw = Arc::new(
        LlmGateway::new(Deps {
            site: "site-1".to_owned(),
            store: store.clone(),
            vault,
            guard: Arc::new(SharedGuard::new(guard)),
            clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
            providers,
            transports: Arc::new(ViaStub(Arc::clone(&stub))),
            redactor: Arc::new(Redactor::from_policy(&RedactionPolicy::default()).unwrap()),
            reader: Arc::new(DenyAllReader),
            max_retries: 0,
            retention_days: 1,
        })
        .with_embed_limits(limits),
    );
    Rig {
        gw,
        store,
        stub,
        resolver,
    }
}

/// A rig whose providers are the real ones, over the scripted egress sender.
fn rig() -> Rig {
    rig_over(Arc::new(BuiltinProviders), EmbedLimits::default())
}

/// A rig whose embedder is the mock.
fn mock_rig(mock: &Arc<MockEmbedder>) -> Rig {
    rig_over(
        Arc::new(MockFactory(Arc::clone(mock))),
        EmbedLimits {
            max_texts: 1_000,
            max_text_bytes: 32_768,
        },
    )
}

/// An OpenRouter tenant that embeds at 8 values, one micro-unit per token.
fn embedder_tenant(tenant: &str) -> TenantConfig {
    TenantConfig {
        tenant: tenant.to_owned(),
        kind: Kind::Openrouter,
        endpoint: "https://api.openrouter.test".to_owned(),
        model: "meta/llama-test".to_owned(),
        allowed_hosts: vec!["api.openrouter.test".to_owned()],
        price: PriceConfig {
            input_micro_per_million: 1_000_000,
            output_micro_per_million: 1_000_000,
        },
        context_window: 100_000,
        max_output: 4_096,
        default_max_output_tokens: 100,
        tasks: vec!["chat".to_owned()],
        budget: None,
        embedding_model: Some("embed-test".to_owned()),
        embedding_dimensions: Some(8),
    }
}

fn caller() -> Caller {
    Caller {
        user: "user:u1".to_owned(),
        purpose: "retrieval:query".to_owned(),
    }
}

async fn ready(rig: &Rig, config: &TenantConfig) {
    rig.gw.put_config(config).await.unwrap();
    rig.gw
        .set_key(&config.tenant, &SecretString::new(KEY))
        .await
        .unwrap();
}

fn texts(items: &[&str]) -> Vec<String> {
    items.iter().map(|t| (*t).to_owned()).collect()
}

/// A 40-byte text, so a call's worst case is 40 micro-units and its settled cost 10.
fn forty(n: usize) -> String {
    let mut text = format!("note-{n}-");
    while text.len() < 40 {
        text.push('x');
    }
    text
}

#[tokio::test]
async fn a_batch_embeds_in_order_and_splits_at_the_provider_limit() {
    let mock = Arc::new(MockEmbedder::new(8));
    let r = mock_rig(&mock);
    ready(&r, &embedder_tenant("acme")).await;

    let input: Vec<String> = (0..300).map(|i| format!("t-{i}")).collect();
    let vectors = r.gw.embed("acme", &caller(), &input).await.unwrap();

    assert_eq!(vectors.len(), 300);
    for (text, vector) in input.iter().zip(&vectors) {
        assert_eq!(vector, &MockEmbedder::vector_for(text, 8));
    }
    // 300 texts are two provider batches, in order, at the configured model and width.
    let requests = mock.requests();
    assert_eq!(
        requests.iter().map(|q| q.inputs.len()).collect::<Vec<_>>(),
        vec![256, 44]
    );
    assert!(
        requests
            .iter()
            .all(|q| q.model == "embed-test" && q.dimensions == Some(8))
    );
    assert_eq!(requests[0].inputs[0], "t-0");
    assert_eq!(requests[1].inputs[0], "t-256");

    // One send record for the call, closed as sent; an empty call does nothing at all.
    let sends = r.gw.sends("acme", 10).await.unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].outcome, "sent");
    assert_eq!(sends[0].task, "embed");
    assert_eq!(sends[0].classes.get("texts"), Some(&300));
    assert!(r.gw.embed("acme", &caller(), &[]).await.unwrap().is_empty());
    assert_eq!(r.gw.sends("acme", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_budget_stops_an_embed_sequence_with_nothing_sent() {
    let mock = Arc::new(MockEmbedder::new(8));
    let r = mock_rig(&mock);
    let mut config = embedder_tenant("acme");
    // Each call's worst case is 40 micro-units (one per byte), its settled cost 10 (the mock
    // counts one token per four bytes): four calls fit, the fifth would pass the cap.
    config.budget = Some(BudgetConfig {
        limit_micro: 70,
        period: Period::Day,
        warn_at_percent: 0,
    });
    ready(&r, &config).await;

    for n in 0..4 {
        r.gw.embed("acme", &caller(), &[forty(n)]).await.unwrap();
    }
    assert_eq!(mock.recorded_count(), 4);
    let refused =
        r.gw.embed("acme", &caller(), &[forty(5)])
            .await
            .unwrap_err();
    assert!(
        matches!(refused, CallError::BudgetExceeded { .. }),
        "{refused}"
    );
    assert_eq!(refused.code(), "llm_host.budget_exceeded");
    assert_eq!(mock.recorded_count(), 4, "the refused call sent nothing");

    let sends = r.gw.sends("acme", 10).await.unwrap();
    assert_eq!(sends.iter().filter(|s| s.outcome == "sent").count(), 4);
    assert!(sends.iter().any(|s| s.outcome == "refused_budget"));
}

#[tokio::test]
async fn an_endpoint_off_the_allow_list_or_refused_by_egress_sends_nothing() {
    let r = rig();

    // A row that reached the store another way: the endpoint is not on its own allow-list.
    let mut off_list = embedder_tenant("acme");
    off_list.endpoint = "https://evil.test".to_owned();
    r.store.put_config(&off_list, 1).await.unwrap();
    r.gw.set_key("acme", &SecretString::new(KEY)).await.unwrap();
    let refused =
        r.gw.embed("acme", &caller(), &texts(&["hello"]))
            .await
            .unwrap_err();
    assert_eq!(refused, CallError::EndpointNotAllowed);

    // A name that resolves to a private address: the egress policy refuses it after DNS.
    let mut private = embedder_tenant("rebinder");
    private.endpoint = "https://rebind.test".to_owned();
    private.allowed_hosts = vec!["rebind.test".to_owned()];
    ready(&r, &private).await;
    let refused =
        r.gw.embed("rebinder", &caller(), &texts(&["hello"]))
            .await
            .unwrap_err();
    assert!(matches!(refused, CallError::EndpointDenied(_)), "{refused}");
    assert_eq!(refused.code(), "llm_host.endpoint_denied");

    assert_eq!(r.stub.calls(), 0, "nothing reached the sender");
    // The refusals are on the record, and the allow-list one did not even resolve a name.
    let sends = r.gw.sends("rebinder", 10).await.unwrap();
    assert!(sends.iter().any(|s| s.outcome == "refused_egress"));
    let sends = r.gw.sends("acme", 10).await.unwrap();
    assert!(sends.iter().any(|s| s.outcome == "refused_allow_list"));
    assert!(r.resolver.total_lookups() >= 1);
}

#[tokio::test]
async fn the_send_record_exists_before_the_call_and_holds_no_text() {
    let r = rig();
    ready(&r, &embedder_tenant("globex")).await;
    r.stub.watch(r.store.clone(), "globex");

    let vectors =
        r.gw.embed("globex", &caller(), &texts(&[SECRET_TEXT, "second note"]))
            .await
            .unwrap();
    assert_eq!(vectors.len(), 2);
    assert!(vectors.iter().all(|v| v.len() == 8));

    // What went out: the documented request, with the text and the key where they belong.
    let seen = r.stub.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].url.ends_with("/v1/embeddings"), "{}", seen[0].url);
    assert!(seen[0].body.contains(SECRET_TEXT));
    assert!(seen[0].body.contains(r#""encoding_format":"float""#));
    assert!(seen[0].body.contains(r#""dimensions":8"#));
    assert!(seen[0].headers.contains(KEY));

    // While the request was in flight the record already existed, as `sending`.
    let during = r.stub.during.lock().unwrap().clone();
    assert_eq!(during.len(), 1);
    assert_eq!(during[0].len(), 1);
    assert_eq!(during[0][0].outcome, "sending");

    // Afterwards it is closed, and nothing in it is a text, a vector or the key.
    let sends = r.gw.sends("globex", 10).await.unwrap();
    assert_eq!(sends.len(), 1);
    let record = &sends[0];
    assert_eq!(record.outcome, "sent");
    assert_eq!(record.endpoint, "https://api.openrouter.test");
    assert_eq!(record.model, "embed-test");
    assert_eq!(record.digest.len(), 64);
    assert_eq!(
        record.size_bytes,
        (SECRET_TEXT.len() + "second note".len()) as u64
    );
    assert_eq!(record.principal, "user:u1");
    assert_eq!(record.purpose, "retrieval:query");
    assert_eq!(record.input_tokens, 14);
    let printed = format!("{sends:?}");
    assert!(!printed.contains(SECRET_TEXT), "{printed}");
    assert!(!printed.contains("second note"), "{printed}");
    assert!(!printed.contains(KEY), "{printed}");
}

#[tokio::test]
async fn a_text_with_a_planted_secret_is_refused_and_not_sent() {
    let r = rig();
    ready(&r, &embedder_tenant("globex")).await;
    let aws = "AKIAIOSFODNN7EXAMPLE";
    let card = "4111 1111 1111 1111";

    let refused =
        r.gw.embed(
            "globex",
            &caller(),
            &texts(&["a harmless note", &format!("the key is {aws} ok")]),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "llm_host.embed_refused");
    let CallError::EmbedRefused { classes } = &refused else {
        panic!("expected a redaction refusal, got {refused}");
    };
    assert!(classes.iter().any(|c| c == "aws_key_id"), "{classes:?}");
    assert!(!refused.to_string().contains(aws), "{refused}");

    let refused =
        r.gw.embed("globex", &caller(), &texts(&[&format!("pay with {card}")]))
            .await
            .unwrap_err();
    let CallError::EmbedRefused { classes } = &refused else {
        panic!("expected a redaction refusal, got {refused}");
    };
    assert!(classes.iter().any(|c| c == "card_number"), "{classes:?}");
    assert!(!refused.to_string().contains("4111"), "{refused}");

    // The whole call was refused: the harmless text did not go either.
    assert_eq!(r.stub.calls(), 0);
    let sends = r.gw.sends("globex", 10).await.unwrap();
    assert_eq!(sends.len(), 2);
    assert!(sends.iter().all(|s| s.outcome == "refused_redaction"));
    let printed = format!("{sends:?}");
    assert!(
        !printed.contains(aws) && !printed.contains("4111"),
        "{printed}"
    );
}

#[tokio::test]
async fn a_call_over_the_bounds_is_refused() {
    let r = rig_over(
        Arc::new(BuiltinProviders),
        EmbedLimits {
            max_texts: 3,
            max_text_bytes: 10,
        },
    );
    ready(&r, &embedder_tenant("globex")).await;
    let too_many =
        r.gw.embed("globex", &caller(), &texts(&["a", "b", "c", "d"]))
            .await
            .unwrap_err();
    assert_eq!(too_many, CallError::EmbedTooLarge);
    assert_eq!(too_many.code(), "llm_host.embed_too_large");
    let too_long =
        r.gw.embed(
            "globex",
            &caller(),
            &texts(&["this text is longer than ten"]),
        )
        .await
        .unwrap_err();
    assert_eq!(too_long, CallError::EmbedTooLarge);
    assert_eq!(r.stub.calls(), 0);
    assert!(
        r.gw.embed("globex", &caller(), &texts(&["fits"]))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_tenant_without_an_embedding_model_answers_not_configured() {
    let r = rig();
    let mut config = embedder_tenant("globex");
    config.embedding_model = None;
    config.embedding_dimensions = None;
    ready(&r, &config).await;
    let refused =
        r.gw.embed("globex", &caller(), &texts(&["hello"]))
            .await
            .unwrap_err();
    assert_eq!(refused, CallError::EmbeddingsNotConfigured);
    assert_eq!(refused.code(), "llm_host.embeddings_not_configured");
    assert_eq!(r.stub.calls(), 0);
    // A tenant that does not exist at all is not configured either.
    let unknown =
        r.gw.embed("nobody", &caller(), &texts(&["hello"]))
            .await
            .unwrap_err();
    assert_eq!(unknown.code(), "llm_host.not_configured");
}

#[tokio::test]
async fn usage_rows_count_embedding_tokens_separately() {
    let r = rig();
    ready(&r, &embedder_tenant("globex")).await;

    // One chat call (12 in, 4 out) and one embedding call of two texts (7 tokens each).
    let (request, manifest) =
        r.gw.prompt("user-1", PromptPolicy::default())
            .text("Add 2 and 3.")
            .build("ignored", "chat-1");
    r.gw.complete("globex", "chat", &caller(), request, &manifest)
        .await
        .unwrap();
    r.gw.embed("globex", &caller(), &texts(&["one", "two"]))
        .await
        .unwrap();
    r.gw.flush_all().await;

    let rows = r.gw.usage("globex").await.unwrap();
    let chat = rows.iter().find(|row| row.task == "chat").unwrap();
    let embed = rows.iter().find(|row| row.task == "embed").unwrap();
    assert_eq!((chat.input, chat.output), (12, 4));
    assert_eq!((embed.input, embed.output, embed.requests), (14, 0, 1));
    assert_eq!(embed.model, "embed-test");
    // One micro-unit per token, exact.
    assert_eq!(embed.cost_micro, 14);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sync_adapter_works_from_a_plain_thread_and_a_blocking_thread() {
    let mock = Arc::new(MockEmbedder::new(8));
    let r = mock_rig(&mock);
    ready(&r, &embedder_tenant("acme")).await;
    let embedder: Arc<dyn MemoryEmbedder> = Arc::new(GatewayEmbedder::new(
        Arc::clone(&r.gw),
        Some("acme".to_owned()),
        8,
        Handle::current(),
    ));
    assert_eq!(embedder.max_batch(), 256);
    let input = texts(&["alpha", "beta"]);

    // A plain OS thread.
    let (tx, rx) = std::sync::mpsc::channel();
    let (plain, plain_input) = (Arc::clone(&embedder), input.clone());
    std::thread::spawn(move || {
        let _ = tx.send(plain.embed(&plain_input));
    });
    let from_thread = tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap()
        .unwrap();

    // A `spawn_blocking` thread of the same multi-thread runtime.
    let (blocking, blocking_input) = (Arc::clone(&embedder), input.clone());
    let from_blocking = tokio::task::spawn_blocking(move || blocking.embed(&blocking_input))
        .await
        .unwrap()
        .unwrap();

    let expected = vec![
        MockEmbedder::vector_for("alpha", 8),
        MockEmbedder::vector_for("beta", 8),
    ];
    assert_eq!(from_thread, expected);
    assert_eq!(from_blocking, expected);
    // Memory's calls are on the record as the named system caller.
    let sends = r.gw.sends("acme", 10).await.unwrap();
    assert_eq!(sends.len(), 2);
    assert!(sends.iter().all(|s| s.principal == "system-agent-memory"));

    // A width the memory store does not have is a wrong shape, never handed on.
    let narrow: Arc<dyn MemoryEmbedder> = Arc::new(GatewayEmbedder::new(
        Arc::clone(&r.gw),
        Some("acme".to_owned()),
        64,
        Handle::current(),
    ));
    let error = tokio::task::spawn_blocking(move || narrow.embed(&texts(&["alpha"])))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("wrong shape"), "{error}");

    // With no tenant set the adapter answers the precise code.
    let unset: Arc<dyn MemoryEmbedder> = Arc::new(GatewayEmbedder::new(
        Arc::clone(&r.gw),
        None,
        8,
        Handle::current(),
    ));
    let error = tokio::task::spawn_blocking(move || unset.embed(&texts(&["alpha"])))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("llm_host.embeddings_not_configured"),
        "{error}"
    );
}

#[test]
fn a_width_mismatch_is_a_degraded_sentence_naming_both_numbers() {
    assert!(check_width("acme", Some(64), 64).is_ok());
    assert!(check_width("acme", None, 64).is_ok());
    let sentence = check_width("acme", Some(8), 64).unwrap_err();
    assert!(sentence.starts_with("llm_host.embed_dimensions_mismatch"));
    assert!(sentence.contains("8 values"), "{sentence}");
    assert!(sentence.contains("64 wide"), "{sentence}");
}

#[tokio::test]
async fn two_tenants_are_independent() {
    let mock = Arc::new(MockEmbedder::new(8));
    let r = mock_rig(&mock);
    let mut acme = embedder_tenant("acme");
    // A cap below one call's worst case: every call of this tenant is refused.
    acme.budget = Some(BudgetConfig {
        limit_micro: 10,
        period: Period::Day,
        warn_at_percent: 0,
    });
    ready(&r, &acme).await;
    ready(&r, &embedder_tenant("globex")).await;

    let refused =
        r.gw.embed("acme", &caller(), &[forty(1)])
            .await
            .unwrap_err();
    assert_eq!(refused.code(), "llm_host.budget_exceeded");
    assert_eq!(mock.recorded_count(), 0);

    // The other tenant is untouched by it and has its own usage and records.
    assert!(r.gw.embed("globex", &caller(), &[forty(2)]).await.is_ok());
    assert_eq!(mock.recorded_count(), 1);
    r.gw.flush_all().await;
    assert!(r.gw.usage("acme").await.unwrap().is_empty());
    assert_eq!(r.gw.usage("globex").await.unwrap().len(), 1);
    assert_eq!(r.gw.sends("acme", 10).await.unwrap().len(), 1);
    assert_eq!(r.gw.sends("globex", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_embed_port_maps_errors_to_codes_and_carries_no_text() {
    let mock = Arc::new(MockEmbedder::new(8));
    let r = mock_rig(&mock);
    let mut acme = embedder_tenant("acme");
    acme.budget = Some(BudgetConfig {
        limit_micro: 10,
        period: Period::Day,
        warn_at_percent: 0,
    });
    ready(&r, &acme).await;
    ready(&r, &embedder_tenant("globex")).await;
    let port = GatewayEmbedPort::new(Arc::clone(&r.gw));
    let request = |provider: &str, principal: Option<&str>, purpose, text: &str| PortRequest {
        provider: provider.to_owned(),
        purpose,
        principal: principal.map(str::to_owned),
        texts: vec![text.to_owned()],
    };

    // A budget error is the stable code, and nothing of the text.
    let failure = port
        .embed(&request(
            "acme",
            None,
            EmbedPurpose::Index,
            "PORT-SECRET-TEXT-xxxxxxxxxxxxxxxxxxxxxxxxx",
        ))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "llm_host.budget_exceeded");
    assert!(!format!("{failure:?}").contains("PORT-SECRET"));

    // The indexer is the named system caller; a query is the user or agent that asked.
    let indexed = port
        .embed(&request("globex", None, EmbedPurpose::Index, "chunk one"))
        .await
        .unwrap();
    assert_eq!(indexed, vec![MockEmbedder::vector_for("chunk one", 8)]);
    port.embed(&request(
        "globex",
        Some("user:u7"),
        EmbedPurpose::Query,
        "a question",
    ))
    .await
    .unwrap();
    port.embed(&request(
        "globex",
        Some("agent:a1"),
        EmbedPurpose::Query,
        "another",
    ))
    .await
    .unwrap();
    let sends = r.gw.sends("globex", 10).await.unwrap();
    let who: Vec<(&str, &str)> = sends
        .iter()
        .map(|s| (s.principal.as_str(), s.purpose.as_str()))
        .collect();
    assert!(
        who.contains(&("system-indexer", "retrieval:index")),
        "{who:?}"
    );
    assert!(who.contains(&("user:u7", "retrieval:query")), "{who:?}");
    assert!(who.contains(&("agent:a1", "retrieval:query")), "{who:?}");

    // A principal of another shape is refused rather than guessed at; a provider name that is
    // not a tenant is a tenant with no embedding setup.
    let failure = port
        .embed(&request("globex", Some("root"), EmbedPurpose::Query, "x"))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "llm_host.config_invalid");
    let failure = port
        .embed(&request("No Such!", None, EmbedPurpose::Index, "x"))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "llm_host.embeddings_not_configured");
    let failure = port
        .embed(&request("nobody", None, EmbedPurpose::Index, "x"))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "llm_host.not_configured");
}

#[tokio::test]
async fn no_key_appears_in_an_error_a_record_or_a_usage_row() {
    let r = rig();
    ready(&r, &embedder_tenant("globex")).await;
    // The provider refuses the key: a provider failure, with the code only.
    r.stub.force_status.store(401, Ordering::SeqCst);
    let refused =
        r.gw.embed("globex", &caller(), &texts(&["hello there"]))
            .await
            .unwrap_err();
    assert_eq!(refused.code(), "llm_host.provider_failed");
    r.stub.force_status.store(0, Ordering::SeqCst);
    r.gw.embed("globex", &caller(), &texts(&["hello there"]))
        .await
        .unwrap();
    r.gw.flush_all().await;

    let sends = r.gw.sends("globex", 10).await.unwrap();
    assert!(sends.iter().any(|s| s.outcome.starts_with("failed_llm")));
    let usage = r.gw.usage("globex").await.unwrap();
    for printed in [
        refused.to_string(),
        format!("{refused:?}"),
        format!("{sends:?}"),
        format!("{usage:?}"),
    ] {
        assert!(!printed.contains(KEY), "{printed}");
        assert!(!printed.contains("hello there"), "{printed}");
    }
    // The key only ever travelled in the provider's own header.
    assert!(r.stub.seen().iter().all(|s| s.headers.contains(KEY)));
}
