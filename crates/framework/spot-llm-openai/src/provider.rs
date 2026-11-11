//! The [`Provider`] implementation for the OpenAI-compatible Chat Completions protocol.

use std::sync::Arc;

use spot_llm::error::LlmError;
use spot_llm::message::{Message, Part, Role};
use spot_llm::provider::{BoxFuture, Capabilities, Provider};
use spot_llm::request::Request;
use spot_llm::response::Response;
use spot_llm::stream::{BoxStream, IterStream, StreamEvent};
use spot_llm::transport::{HttpRequest, HttpTransport, TransportError};

use crate::close::close_arguments;
use crate::errors::{RETRY_AFTER_HEADER, map, retry_after_seconds};
use crate::finish_reason;
use crate::profile::{AuthStyle, Profile};
use crate::sse;
use crate::wire;

/// The default base URL: the host only; `{base}/v1/chat/completions` is appended —
/// so an empty base URL sends to the canonical OpenAI endpoint.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// The path Chat Completions lives at, appended to the base URL (the protocol's own
/// `/v1` prefix included, so the caller's base URL is just the host or host plus a
/// deployment prefix).
const CHAT_PATH: &str = "/v1/chat/completions";

/// One model in the provider's catalogue, passed in as data — never hard-coded names with
/// behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The model id as the endpoint spells it (e.g. `gpt-4o`, `llama3.2`, or an
    /// OpenRouter route like `openai/gpt-4o`).
    pub id: String,
    /// The context window in tokens.
    pub context_window: u64,
    /// The model's largest supported generation in tokens.
    pub max_output: u64,
    /// Whether the model accepts images.
    pub vision: bool,
}

impl ModelInfo {
    /// The cached capabilities for this model.
    fn capabilities(&self, profile: &Profile) -> Capabilities {
        Capabilities {
            context_window: self.context_window,
            max_output: self.max_output,
            tools: profile.supports_tools,
            streaming: true,
            vision: self.vision,
            prompt_caching: false,
        }
    }
}

/// How many characters of a base URL an error detail may quote (host only, never the path,
/// never a query that could carry a key).
fn base_url_echo(base_url: &str) -> String {
    base_url
        .split(['/', '?', '#'])
        .take(3)
        .collect::<Vec<_>>()
        .join("/")
}

/// The OpenAI-compatible Chat Completions provider: [`Provider`] over an injected
/// [`HttpTransport`], profiled for one endpoint.
///
/// Nothing here reads the environment; the key arrives as an [`spot_llm::ApiKey`] (a key
/// passed under a `None`-auth profile is ignored — the endpoint trusts its caller), the
/// base URL as a constructor argument, the model catalogue as data. The endpoint is
/// `{base}/chat/completions`, sent exactly — so a recorded-exchange test that needs
/// several different replies keys its recordings by distinct base URLs (e.g.
/// `https://openrouter.test`), which the constructor accepts like any other.
pub struct OpenAiProvider {
    profile: Profile,
    base_url: String,
    api_key: Option<spot_llm::ApiKey>,
    transport: Arc<dyn HttpTransport>,
    models: Vec<ModelInfo>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key and the transport print only as their presence, never their value.
        f.debug_struct("OpenAiProvider")
            .field("base_url", &base_url_echo(&self.base_url))
            .field("api_key", &self.api_key.as_ref().map_or("none", |_| "***"))
            .field("transport", &"Arc<dyn HttpTransport>")
            .field("models", &self.models.len())
            .finish()
    }
}

impl OpenAiProvider {
    /// Builds the provider: `profile` (what the endpoint supports), `base_url` (default
    /// [`DEFAULT_BASE_URL`] when empty), the optional key, the injected transport, and the
    /// model catalogue as data. A `Bearer` profile with no key builds fine and fails
    /// per-request with `llm.auth` — the failure mode BYOK callers hit.
    pub fn new(
        profile: Profile,
        base_url: impl Into<String>,
        api_key: Option<spot_llm::ApiKey>,
        transport: Arc<dyn HttpTransport>,
        models: Vec<ModelInfo>,
    ) -> Self {
        let base_url = {
            let candidate = base_url.into();
            if candidate.is_empty() {
                DEFAULT_BASE_URL.to_owned()
            } else {
                candidate
            }
        };
        Self {
            profile,
            base_url,
            api_key,
            transport,
            models,
        }
    }

    /// The endpoint URL for one call.
    fn endpoint(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        format!("{trimmed}{CHAT_PATH}")
    }

    /// The base URL as the debug echo prints it (host only, no path or query). The
    /// integration tests read it to interpret routing misses.
    pub(crate) fn debug_base_url(&self) -> String {
        base_url_echo(&self.base_url)
    }

    /// The base URL echo under a name the integration tests reach (`pub(crate)` is not
    /// visible across the crate boundary to a test target).
    #[doc(hidden)]
    pub fn debug_base_url_public(&self) -> String {
        self.debug_base_url()
    }

    /// The profile the provider holds.
    pub(crate) fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The model catalogue the provider holds.
    pub(crate) fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    /// Builds the outbound HTTP request: the profile's auth and extra headers, the
    /// `request-id` header for correlation, and the mapped JSON body — `stream` set only
    /// when `streaming` asks.
    pub(crate) fn outbound(
        &self,
        spot_request: &Request,
        streaming: bool,
    ) -> Result<HttpRequest, LlmError> {
        let built = wire::outbound(
            spot_request,
            &self.profile,
            self.models.as_slice(),
            streaming,
        )
        .map_err(|build_error| match build_error {
            wire::BuildError::UnknownModel(model) => LlmError::BadRequest {
                detail: format!("model '{model}' is not in the provider's model catalogue"),
            },
            wire::BuildError::ToolsUnsupported => LlmError::BadRequest {
                detail: "the endpoint's profile does not support tools".to_owned(),
            },
            wire::BuildError::Temperature(permille) => LlmError::BadRequest {
                detail: format!("temperature {permille} is outside the 0..=1000 permille range"),
            },
        })?;
        // Serialization of a built body cannot fail: every field is String/number/Value.
        let body = match serde_json::to_string(&built) {
            Ok(body) => body,
            Err(_) => return Err(protocol("the outbound request could not be serialized")),
        };
        let mut outbound = HttpRequest::post_json(self.endpoint(), body)
            .with_header("request-id", &spot_request.request_id);
        match (&self.profile.auth, &self.api_key) {
            (AuthStyle::Bearer, Some(key)) => {
                let value = format!("Bearer {}", key.expose());
                outbound = outbound.with_header("authorization", &value);
            }
            (AuthStyle::Bearer, None) => return Err(LlmError::Auth),
            // A None-auth profile ignores a key; the endpoint trusts its caller.
            (AuthStyle::None, _) => {}
        }
        for (name, value) in &self.profile.extra_headers {
            outbound = outbound.with_header(name, value);
        }
        Ok(outbound)
    }

    /// Maps a failed HTTP exchange onto [`LlmError`]: transport errors keep their meaning;
    /// status responses parse their envelope; `retry-after` is honoured where defined.
    fn error_from(&self, status: u16, headers: &[(String, String)], body: &str) -> LlmError {
        let retry_after = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(RETRY_AFTER_HEADER))
            .and_then(|(_, value)| retry_after_seconds(value));
        map(status, body, retry_after)
    }

    /// Maps a buffered Chat Completions reply onto the spot-llm types: a 200 body that is
    /// the endpoint's error object maps through [crate::errors]; a success parses its
    /// choices, tool calls and usage.
    fn response_from(
        &self,
        http: &spot_llm::transport::HttpResponse,
    ) -> Result<Response, LlmError> {
        if http.status != 200 {
            return Err(self.error_from(http.status, &http.headers, &http.body));
        }
        self.complete_body(&http.body)
    }

    /// Maps a buffered 200 Chat Completions JSON body onto a [`Response`]. A body that is
    /// the endpoint's error object (a 200-status error) maps identically to a failed
    /// status — through [`crate::errors::map`].
    fn complete_body(&self, body: &str) -> Result<Response, LlmError> {
        let parsed: wire::Inbound = serde_json::from_str(body)
            .map_err(|_| protocol("the response body was not the Chat Completions JSON"))?;
        if parsed.is_error() {
            let entry = parsed
                .error_entry()
                .ok_or_else(|| protocol("the response carried an error object without fields"))?;
            // Re-envelope the entry, so the same status-plus-body mapping decides the
            // error's code for a body that shipped its error object under a 200 status.
            let body_text = serde_json::to_string(&ErrorEnvelope {
                error: Some(entry.clone()),
            })
            .map_err(|_| protocol("the endpoint's error object could not be read"))?;
            return Err(self.error_from(200, &[], &body_text));
        }
        let Some(choice) = parsed.choices.first() else {
            return Err(protocol("the reply carried no choices"));
        };
        let Some(message) = choice.message.as_ref() else {
            return Err(protocol("the reply's choice carried no message"));
        };
        let reason = match choice.finish_reason.as_deref() {
            Some(name) => finish_reason::map(name).map_err(protocol)?,
            None => {
                return Err(protocol("the reply's choice carried no finish_reason"));
            }
        };
        let mut parts: Vec<Part> = Vec::new();
        if let Some(text) = message.content.as_deref()
            && !text.is_empty()
        {
            parts.push(Part::Text {
                text: text.to_owned(),
            });
        }
        if let Some(calls) = message.tool_calls.as_deref() {
            for call in calls {
                let id = call
                    .id
                    .clone()
                    .ok_or_else(|| protocol("a tool call carried no id"))?;
                let name = call
                    .function
                    .as_ref()
                    .and_then(|function| function.name.clone())
                    .ok_or_else(|| protocol("a tool call carried no function name"))?;
                let raw = call
                    .function
                    .as_ref()
                    .and_then(|function| function.arguments.clone())
                    .unwrap_or_default();
                let arguments = parse_arguments(&raw, self.profile.tool_args_may_be_partial_json)
                    .ok_or_else(|| {
                    protocol(format!(
                        "the tool call '{name}' arguments were malformed and the profile \
                             does not parse partial JSON"
                    ))
                })?;
                parts.push(Part::ToolUse {
                    id,
                    name,
                    arguments,
                });
            }
        }
        Ok(Response::new(
            Message {
                role: Role::Assistant,
                parts,
            },
            reason,
            parsed.usage(),
        ))
    }
}

/// The `{"error": ...}` envelope a 200-status error body is re-read through, so the same
/// status-mapping code decides the error's code.
#[derive(serde::Serialize)]
struct ErrorEnvelope {
    error: Option<crate::wire::ErrorEntry>,
}

/// Parses a tool-call arguments string: strictly, or — where the profile says the
/// endpoint's models may cut arguments short — through the opt-in partial-JSON closer.
fn parse_arguments(raw: &str, lenient: bool) -> Option<serde_json::Value> {
    if lenient {
        close_arguments(raw)
    } else {
        serde_json::from_str(raw).ok()
    }
}

/// A `llm.protocol` error with `detail`.
fn protocol(detail: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        detail: detail.into(),
    }
}

impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai-compatible"
    }

    fn capabilities(&self, model: &str) -> Capabilities {
        self.models
            .iter()
            .find(|candidate| candidate.id == model)
            .map(|known| known.capabilities(&self.profile))
            .unwrap_or_else(|| {
                // The provider's floor: models outside the catalogue still report something
                // positive to plan with (the conformance caps scenario requires it).
                Capabilities {
                    context_window: 1,
                    max_output: 1,
                    tools: self.profile.supports_tools,
                    streaming: true,
                    vision: false,
                    prompt_caching: false,
                }
            })
    }

    fn complete<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(async move {
            let outbound = self.outbound(&request, false)?;
            let http = match self.transport.send(outbound).await {
                Ok(http) => http,
                Err(transport_error) => return Err(transport_map(transport_error)),
            };
            self.response_from(&http)
        })
    }

    fn stream<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<BoxStream, LlmError>> {
        Box::pin(async move {
            let outbound = self.outbound(&request, true)?;
            // The stream is requested (`stream: true` in the body) and READ BUFFERED: the
            // transport's typed failure set already carries everything this layer needs,
            // and a reply can fail exactly like a buffered call (auth, rate limit,
            // overload), which only a status check can see. Every failure after this point
            // surfaces as a stream item, not a call error — the conformance error scenario
            // reads it off the stream, and the channel is already committed.
            let failure =
                |error: LlmError| Ok(Box::pin(IterStream::new(vec![Err(error)])) as BoxStream);
            let http = match self.transport.send(outbound).await {
                Ok(http) => http,
                Err(transport_error) => return failure(transport_map(transport_error)),
            };
            if http.status != 200 {
                return failure(self.error_from(http.status, &http.headers, &http.body));
            }
            // SSE when the endpoint says so; otherwise the buffered JSON reply is replayed
            // as its events, so "stream equals complete" holds against any endpoint that
            // answers both call shapes with plain responses.
            let is_sse = http.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("content-type")
                    && value
                        .trim()
                        .to_ascii_lowercase()
                        .starts_with("text/event-stream")
            });
            let events = if is_sse {
                sse::parse(&http.body)
            } else {
                self.complete_body(&http.body)
                    .map(|response| events_of(&response))
            };
            match events {
                Ok(events) => Ok(Box::pin(IterStream::new(
                    events.into_iter().map(Ok).collect::<Vec<_>>(),
                )) as BoxStream),
                Err(error) => failure(error),
            }
        })
    }
}

/// Maps a transport failure onto [`LlmError`] through its documented meaning, never its
/// text (the transport's own detail quotes URLs, which are not for error surfaces here).
fn transport_map(error: TransportError) -> LlmError {
    match error {
        TransportError::Timeout => LlmError::Timeout,
        TransportError::Connection(_) => LlmError::Unavailable,
        TransportError::TooLarge { .. } => LlmError::Protocol {
            detail: "the response body exceeded the transport's buffered cap".to_owned(),
        },
        TransportError::Protocol(_) => LlmError::Protocol {
            detail: "the exchange broke the HTTP framing".to_owned(),
        },
    }
}

/// Builds the event list a parsed response replays as, so the stream and the buffered
/// reply agree by construction (the conformance suite's "stream equals complete").
pub(crate) fn events_of(response: &Response) -> Vec<StreamEvent> {
    let mut events: Vec<StreamEvent> = Vec::new();
    for part in &response.message.parts {
        match part {
            Part::Text { text } => events.push(StreamEvent::TextDelta {
                delta: text.clone(),
            }),
            Part::ToolUse {
                id,
                name,
                arguments,
            } => {
                let index = next_index(&events);
                events.push(StreamEvent::ToolUseStart {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                });
                events.push(StreamEvent::ToolUseDelta {
                    index,
                    delta: arguments.to_string(),
                });
                events.push(StreamEvent::ToolUseEnd { index });
            }
            Part::ToolResult { .. } => {}
        }
    }
    events.push(StreamEvent::Usage {
        usage: response.usage,
    });
    events.push(StreamEvent::Stop {
        reason: response.stop_reason,
    });
    events
}

/// The next free tool-call index for a synthetic bracket.
fn next_index(events: &[StreamEvent]) -> u32 {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolUseEnd { index } => Some(*index + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}
