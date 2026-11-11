//! Shared helpers for the `spot-llm-openai` integration tests: the per-profile benches,
//! the provider builds over recorded fixtures and their per-scenario base URLs, the
//! request-id-routing transport the conformance suite runs through, and the stream
//! drain/assemble conveniences.
//!
//! The helpers are shared by several test targets; a target that does not use one
//! triggers no dead-code error, so the module carries `allow(dead_code)` — the cargo
//! book's own advice for test `common` modules.
#![allow(dead_code)]

use std::sync::Arc;

use spot_llm::conformance::Fixture;
use spot_llm::message::Message;
use spot_llm::provider::{BoxFuture, Provider as _};
use spot_llm::request::Request;
use spot_llm::response::{Response, StopReason};
use spot_llm::stream::{StreamEvent, assemble, drain};
use spot_llm::transport::mock::{MockTransport, RecordedExchange};
use spot_llm_openai::test_hooks::outbound_for;
use spot_llm_openai::{ModelInfo, OpenAiProvider, Profile};

/// The conformance fixture oracle, retargeted at the model id each profile's recordings
/// carry (the wire `model` must equal the request's for the fixture to be coherent).
pub fn fixture_for(model: &str) -> Fixture {
    let mut sample = Fixture::sample();
    sample.model = model.to_owned();
    sample
}

/// The per-profile setup: the profile, the model id, and how to key the recordings.
pub struct Bench {
    /// The scenario name (also the fixture file's stem).
    pub name: &'static str,
    /// The profile the provider is built with.
    pub profile: Profile,
    /// The model id the fixture requests and the recordings answer.
    pub model: &'static str,
}

/// A bench built from its parts.
pub fn bench_custom(name: &'static str, profile: Profile, model: &'static str) -> Bench {
    Bench {
        name,
        profile,
        model,
    }
}

/// The bench whose fixture file is `name`'s.
pub fn bench_by_name(name: &str) -> Bench {
    benches()
        .into_iter()
        .find(|bench| bench.name == name)
        .unwrap_or_else(|| panic!("bench '{name}' is one of the three"))
}

/// The three benches: OpenRouter, Ollama, and a generic compatible endpoint.
pub fn benches() -> Vec<Bench> {
    vec![
        bench_custom("openrouter", Profile::openrouter(), "openai/gpt-4o"),
        bench_custom("ollama", Profile::ollama(), "llama3.2"),
        bench_custom("generic", Profile::generic(), "gpt-4o"),
    ]
}

/// The generic profile, spelled out for the wire-shape tests.
pub fn generic_profile() -> Profile {
    Profile::generic()
}

/// The model catalogue the tests build providers with, keyed per bench model id.
pub fn benchmark_models(model: &str) -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: model.to_owned(),
        context_window: 128_000,
        max_output: 4_096,
        vision: true,
    }]
}

/// Which key suits a profile: a fake one under bearer auth, none otherwise.
pub fn key_for(profile: &Profile) -> Option<spot_llm::ApiKey> {
    if profile.auth != spot_llm_openai::AuthStyle::Bearer {
        return None;
    }
    Some(spot_llm::ApiKey::new("test-key-not-real"))
}

/// Builds a provider: profile, base URL (empty takes the default), key, transport, model
/// catalogue — the one build path the tests assemble through.
pub fn build_t(
    profile: &Profile,
    base_url: &str,
    key: Option<spot_llm::ApiKey>,
    transport: Arc<dyn spot_llm::transport::HttpTransport>,
    model: &str,
) -> OpenAiProvider {
    OpenAiProvider::new(
        profile.clone(),
        base_url,
        key,
        transport,
        benchmark_models(model),
    )
}

/// Builds a provider over an empty replaying transport — the pre-transport scenarios
/// (build failures, header assertions, default base URL) use it.
pub fn provider_nowhere(profile: &Profile, base_url: &str, model: &str) -> OpenAiProvider {
    build_t(
        profile,
        base_url,
        key_for(profile),
        Arc::new(MockTransport::new(Vec::<RecordedExchange>::new())),
        model,
    )
}

/// Builds a provider over the replaying transport loaded from the bench's fixture file,
/// at the base URL the tested exchange was recorded under.
pub fn provider_at(base_url: &str, bench: &Bench) -> OpenAiProvider {
    let transport = transport_of(bench);
    build_t(
        &bench.profile,
        base_url,
        key_for(&bench.profile),
        Arc::new(transport),
        bench.model,
    )
}

/// Loads the bench's recording.
pub fn transport_of(bench: &Bench) -> MockTransport {
    let file = format!("tests/fixtures/{}.json", bench.name);
    let path = format!("{}/{file}", env!("CARGO_MANIFEST_DIR"));
    MockTransport::load_file(&path)
        .unwrap_or_else(|error| panic!("fixture {} is a broken recording: {error}", bench.name))
}

/// The base URL whose recording answers the plain-completion scenario, per bench.
pub fn base_text(bench: &Bench) -> &'static str {
    match bench.name {
        "openrouter" => "https://openrouter.test",
        "ollama" => "https://ollama.test",
        _ => "https://generic.test",
    }
}

/// The streaming variant's base, per bench.
pub fn base_stream(bench: &Bench) -> &'static str {
    match bench.name {
        "openrouter" => "https://or-stream.test",
        "ollama" => "https://ollama-stream.test",
        _ => "https://generic-stream.test",
    }
}

/// The tool-call leg's base, per bench.
pub fn base_tool(bench: &Bench) -> &'static str {
    match bench.name {
        "openrouter" => "https://or-tool.test",
        "ollama" => "https://ollama-tool.test",
        _ => "https://generic-tool.test",
    }
}

/// The tool-result leg's base, per bench.
pub fn base_tool_result(bench: &Bench) -> &'static str {
    match bench.name {
        "openrouter" => "https://or-tool-result.test",
        "ollama" => "https://ollama-tool-result.test",
        _ => "https://generic-tool-result.test",
    }
}

/// The streaming tool-call variant's base, per bench.
pub fn base_stream_tool(bench: &Bench) -> &'static str {
    match bench.name {
        "openrouter" => "https://or-stream-tool.test",
        "ollama" => "https://ollama-stream-tool.test",
        _ => "https://generic-stream-tool.test",
    }
}

/// The outbound HTTP request for `request`, exactly as `complete` would send it. Failing
/// builds panic: the tests only send buildable requests.
pub fn outbound_of(
    provider: &OpenAiProvider,
    request: &Request,
    streaming: bool,
) -> spot_llm::transport::HttpRequest {
    outbound_for(provider, request, streaming)
}

/// Drains a provider stream into its events, failing on a failed open or an error item.
pub async fn events_from(
    provider: &OpenAiProvider,
    request: Request,
    what: &str,
) -> Vec<StreamEvent> {
    let mut stream = match provider.stream(request).await {
        Ok(stream) => stream,
        Err(error) => panic!("{what} must open: {error}"),
    };
    let mut events = Vec::new();
    for item in drain(&mut stream).await {
        match item {
            Ok(event) => events.push(event),
            Err(error) => panic!("{what} must not fail mid-way: {error}"),
        }
    }
    events
}

/// Assembles `events`, failing on an unusable fold.
pub fn must_assemble(events: &[StreamEvent], what: &str) -> Response {
    match assemble(events) {
        Ok(response) => response,
        Err(error) => panic!("{what} must assemble: {error}"),
    }
}

/// The expected response of the fixture's plain scenarios.
pub fn expected_text_response(fixture: &Fixture) -> Response {
    Response::new(
        Message::assistant(fixture.text.reply.clone()),
        StopReason::Stop,
        fixture.text.usage,
    )
}

/// The routed transport: matches a recording by the `request-id` header and the `stream`
/// body flag — the two discriminating parts of the provider's outbound request (the URL
/// is one endpoint per provider instance, so the header must discriminate). It answers
/// either call shape: a `send_stream` receives the recorded stream's chunks.
pub struct RoutingTransport {
    exchanges: Vec<RecordedExchange>,
}

impl RoutingTransport {
    /// Wraps `exchanges`, dropping any whose recorded URL names a base the suite does not
    /// run at (`skip_url_part`), so a bench's extra recordings cannot shadow a scenario.
    pub fn new(exchanges: Vec<RecordedExchange>, skip_url_part: &str) -> Self {
        Self {
            exchanges: exchanges
                .into_iter()
                .filter(|exchange| !exchange.request.url.contains(skip_url_part))
                .collect(),
        }
    }

    /// The routing transport over a bench's recording, less its truncated-arguments
    /// base (the recorded reply for that base is a scenario's, not the suite's).
    pub fn over_bench(bench: &Bench) -> Self {
        let recording = transport_of(bench);
        Self::new(recording.exchanges().to_vec(), "truncated")
    }

    fn pick(&self, request: &spot_llm::transport::HttpRequest) -> Option<&RecordedExchange> {
        let request_id = request
            .headers
            .iter()
            .find(|(name, _)| name == "request-id")
            .map(|(_, value)| value.as_str());
        let body: serde_json::Value = serde_json::from_str(&request.body).ok()?;
        let streaming = body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_default();
        self.exchanges.iter().find(|exchange| {
            let recorded_id = exchange
                .request
                .headers
                .iter()
                .find(|(name, _)| name == "request-id")
                .map(|(_, value)| value.as_str());
            let recorded_body: Option<serde_json::Value> =
                serde_json::from_str(&exchange.request.body).ok();
            let recorded_stream = recorded_body
                .as_ref()
                .and_then(|body| body.get("stream"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_default();
            recorded_id == request_id && recorded_stream == streaming
        })
    }

    fn miss(
        &self,
        request: &spot_llm::transport::HttpRequest,
    ) -> spot_llm::transport::TransportError {
        spot_llm::transport::TransportError::Protocol(format!(
            "no recorded exchange for {} {}",
            request.method.as_str(),
            request.url
        ))
    }
}

impl spot_llm::transport::HttpTransport for RoutingTransport {
    fn send<'a>(
        &'a self,
        request: spot_llm::transport::HttpRequest,
    ) -> BoxFuture<'a, Result<spot_llm::transport::HttpResponse, spot_llm::transport::TransportError>>
    {
        Box::pin(async move {
            match self.pick(&request) {
                Some(exchange) => match &exchange.response {
                    spot_llm::transport::mock::RecordedResponse::Complete {
                        status,
                        headers,
                        body,
                    } => Ok(spot_llm::transport::HttpResponse {
                        status: *status,
                        headers: headers.clone(),
                        body: body.clone(),
                    }),
                    spot_llm::transport::mock::RecordedResponse::Stream {
                        status,
                        headers,
                        chunks,
                    } => Ok(spot_llm::transport::HttpResponse {
                        status: *status,
                        headers: headers.clone(),
                        body: chunks.concat(),
                    }),
                },
                None => Err(self.miss(&request)),
            }
        })
    }

    fn send_stream<'a>(
        &'a self,
        request: spot_llm::transport::HttpRequest,
    ) -> BoxFuture<'a, Result<spot_llm::transport::ByteStream, spot_llm::transport::TransportError>>
    {
        Box::pin(async move {
            let items: Vec<Result<Vec<u8>, spot_llm::transport::TransportError>> = match self
                .pick(&request)
            {
                Some(exchange) => match &exchange.response {
                    spot_llm::transport::mock::RecordedResponse::Complete { body, .. } => {
                        vec![Ok(body.clone().into_bytes())]
                    }
                    spot_llm::transport::mock::RecordedResponse::Stream { chunks, .. } => chunks
                        .iter()
                        .map(|chunk| Ok(chunk.clone().into_bytes()))
                        .collect(),
                },
                None => return Err(self.miss(&request)),
            };
            Ok(Box::pin(spot_llm::stream::IterStream::new(items))
                as spot_llm::transport::ByteStream)
        })
    }
}
