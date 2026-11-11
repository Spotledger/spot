//! The [`Provider`] implementation for the Anthropic Messages API.
//!
//! The provider builds one `POST {base}/v1/messages` HTTP request per call — headers
//! (`x-api-key`, `anthropic-version`, a redacting-safe request id), a mapped JSON body — and
//! maps the buffered reply or the SSE stream back onto the `spot-llm` types. Errors map in
//! [`crate::errors`]; the SSE parsing lives in [`crate::sse`].

use std::sync::Arc;

use spot_llm::error::LlmError;
use spot_llm::message::{Message, Part, Role};
use spot_llm::provider::{BoxFuture, Capabilities, Provider};
use spot_llm::request::Request;
use spot_llm::response::Response;
use spot_llm::stream::{BoxStream, IterStream, StreamEvent};
use spot_llm::transport::{HttpRequest, HttpResponse, HttpTransport, TransportError};

use crate::cache::CacheMarker;
use crate::errors::{RETRY_AFTER_HEADER, map, retry_after_seconds};
use crate::sse;
use crate::wire;

/// The default base URL (`https://api.anthropic.com`).
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The `anthropic-version` header value the API documents.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The path the Messages API lives at, appended to the base URL.
const MESSAGES_PATH: &str = "/v1/messages";

/// How many characters of a base URL an error detail may quote (host only, never the path,
/// never a query that could carry a key).
fn base_url_echo(base_url: &str) -> String {
    base_url
        .split(['/', '?', '#'])
        .take(3)
        .collect::<Vec<_>>()
        .join("/")
}

/// One model in the provider's catalogue, passed in as data — never hard-coded names with
/// behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The model id as the API spells it (e.g. `claude-sonnet-4-5`).
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
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            context_window: self.context_window,
            max_output: self.max_output,
            tools: true,
            streaming: true,
            vision: self.vision,
            prompt_caching: true,
        }
    }
}

/// A request the agent asked for, plus the provider-level options Anthropic's API needs: the
/// required `max_tokens` and the opt-in prompt-caching markers.
///
/// Carried beside the [`Request`] (which stays provider-neutral) rather than grown into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnthropicOptions {
    /// Overrides the model's `max_output` for the required `max_tokens` field.
    pub max_tokens: Option<u32>,
    /// Which prompt prefixes to mark for caching.
    pub cache: CacheMarker,
}

impl Default for AnthropicOptions {
    fn default() -> Self {
        Self {
            max_tokens: None,
            cache: CacheMarker::None,
        }
    }
}

/// The Anthropic Messages API provider: [`Provider`] over an injected [`HttpTransport`].
///
/// Nothing here reads the environment; the key arrives as an [`spot_llm::ApiKey`], the base
/// URL as a constructor argument, the model catalogue as data. The endpoint is
/// `{base}/v1/messages`, sent exactly — so a recorded-exchange test that needs several
/// different replies keys its recordings by distinct base URLs (e.g. `https://rate-limit.
/// test`), which the constructor accepts like any other.
pub struct AnthropicProvider {
    base_url: String,
    api_key: spot_llm::ApiKey,
    transport: Arc<dyn HttpTransport>,
    models: Vec<ModelInfo>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key and the transport print only as their presence, never their value.
        f.debug_struct("AnthropicProvider")
            .field("base_url", &base_url_echo(&self.base_url))
            .field("api_key", &"***")
            .field("transport", &"Arc<dyn HttpTransport>")
            .field("models", &self.models.len())
            .finish()
    }
}

impl AnthropicProvider {
    /// Builds the provider: `base_url` (default [`DEFAULT_BASE_URL`] when empty), the key, the
    /// injected transport, and the model catalogue as data.
    pub fn new(
        base_url: impl Into<String>,
        api_key: spot_llm::ApiKey,
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
            base_url,
            api_key,
            transport,
            models,
        }
    }

    /// The endpoint URL for one call.
    fn endpoint(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        format!("{trimmed}{MESSAGES_PATH}")
    }

    /// The base URL as the debug echo prints it (host only, no path or query).
    pub(crate) fn debug_base_url(&self) -> String {
        base_url_echo(&self.base_url)
    }

    /// Builds the outbound HTTP request. `request_id` goes in a `request-id` header —
    /// never a URL query — so an endpoint can correlate without the body's contents;
    /// `streaming` sets the wire body's `stream` flag (both call shapes post the same
    /// endpoint, and the recorded exchanges distinguish them by it).
    pub(crate) fn outbound(
        &self,
        spot_request: &Request,
        options: &AnthropicOptions,
        streaming: bool,
    ) -> Result<HttpRequest, LlmError> {
        let built =
            wire::Outbound::from_request(spot_request, options, self.models.as_slice(), streaming)
                .map_err(|build_error| match build_error {
                    wire::BuildError::UnknownModel(model) => LlmError::BadRequest {
                        detail: format!("model '{model}' is not in the provider's model catalogue"),
                    },
                    wire::BuildError::MaxTokens(cap) => LlmError::BadRequest {
                        detail: format!(
                            "max_tokens does not fit the model's {cap}-token output cap"
                        ),
                    },
                    wire::BuildError::Temperature(permille) => LlmError::BadRequest {
                        detail: format!(
                            "temperature {permille} is outside the 0..=1000 permille range"
                        ),
                    },
                })?;
        // Serialization of a built body cannot fail: every field is String/number/Value.
        let body = match serde_json::to_string(&built) {
            Ok(body) => body,
            Err(_) => {
                return Err(protocol("the outbound request could not be serialized"));
            }
        };
        Ok(HttpRequest::post_json(self.endpoint(), body)
            .with_header("x-api-key", self.api_key.expose())
            .with_header("anthropic-version", ANTHROPIC_VERSION)
            .with_header("request-id", &spot_request.request_id))
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

    /// Maps a buffered HTTP response onto the spot-llm types: errors parse their envelope,
    /// successes their content blocks and usage.
    fn response_from(&self, http: &HttpResponse) -> Result<Response, LlmError> {
        if http.status != 200 {
            return Err(self.error_from(http.status, &http.headers, &http.body));
        }
        let parsed: wire::Inbound = serde_json::from_str(&http.body)
            .map_err(|_| protocol("the response body was not the Messages API's JSON"))?;
        let reason = match parsed.stop_reason.as_deref() {
            Some("end_turn") | None => spot_llm::response::StopReason::Stop,
            Some("max_tokens") => spot_llm::response::StopReason::Length,
            Some("tool_use") => spot_llm::response::StopReason::ToolUse,
            Some("refusal") => spot_llm::response::StopReason::ContentFilter,
            Some(other) => {
                return Err(protocol(format!("unknown stop_reason '{other}'")));
            }
        };
        let mut parts: Vec<Part> = Vec::new();
        for block in &parsed.content {
            match block.kind.as_str() {
                "text" => {
                    if let Some(text) = block.text.as_deref()
                        && !text.is_empty()
                    {
                        parts.push(Part::Text {
                            text: text.to_owned(),
                        });
                    }
                }
                "tool_use" => parts.push(Part::ToolUse {
                    id: block.id.clone().unwrap_or_default(),
                    name: block.name.clone().unwrap_or_default(),
                    arguments: block.input.clone().unwrap_or(serde_json::Value::Null),
                }),
                // Unknown block kinds are tolerated and skipped.
                _ => {}
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

/// A `llm.protocol` error with `detail`.
fn protocol(detail: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        detail: detail.into(),
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self, model: &str) -> Capabilities {
        self.models
            .iter()
            .find(|candidate| candidate.id == model)
            .map(ModelInfo::capabilities)
            .unwrap_or_else(|| {
                // The provider's floor: models outside the catalogue still report something
                // positive to plan with (the conformance caps scenario requires it).
                Capabilities {
                    context_window: 1,
                    max_output: 1,
                    tools: true,
                    streaming: true,
                    vision: false,
                    prompt_caching: true,
                }
            })
    }

    fn complete<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(async move {
            let outbound = self.outbound(&request, &AnthropicOptions::default(), false)?;
            let http = match self.transport.send(outbound).await {
                Ok(http) => http,
                Err(transport_error) => return Err(transport_map(transport_error)),
            };
            self.response_from(&http)
        })
    }

    fn stream<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<BoxStream, LlmError>> {
        Box::pin(async move {
            let outbound = self.outbound(&request, &AnthropicOptions::default(), true)?;
            // The stream is requested (`stream: true` in the body) and READ BUFFERED: the
            // transport's typed failure set already carries everything this layer needs, and
            // a reply can fail exactly like a buffered call (auth, rate limit, overload),
            // which only a status check can see. Every failure after this point surfaces as
            // a stream item, not a call error — the conformance error scenario reads it off
            // the stream, and the channel is already committed.
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
                sse::parse_events(&http.body)
            } else {
                self.response_from(&http)
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

/// Maps a transport failure onto [`LlmError`] through its documented meaning, never its text
/// (the transport's own detail quotes URLs, which are not for error surfaces here).
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

/// Builds the event list a parsed response replays as, so the stream and the buffered reply
/// agree by construction (the conformance suite's "stream equals complete").
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
