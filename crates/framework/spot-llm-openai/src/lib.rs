//! `spot-llm-openai`: the OpenAI-compatible Chat Completions provider (WP-AI-03.T3; ADR-26).
//!
//! [`OpenAiProvider`] implements [`spot_llm::Provider`] for `/v1/chat/completions` — the
//! protocol OpenRouter, Ollama, llama.cpp server, vLLM, LM Studio and every other OpenAI-
//! compatible endpoint speaks — over an injected [`spot_llm::HttpTransport`]. Compatible
//! servers differ, so the provider takes a [`Profile`] stating what the endpoint supports
//! (auth, extra headers, tools, streaming usage, the max-tokens field name, lenient tool
//! arguments); the constructors [`Profile::openrouter`], [`Profile::ollama`] and
//! [`Profile::generic`] ship the common shapes. Bring-your-own-key ([`spot_llm::ApiKey`]), a
//! configurable base URL, and a model catalogue passed in as data ([`ModelInfo`]) — nothing
//! reads the environment.
//!
//! The pieces:
//! - [`profile`]: the endpoint profile as plain data.
//! - [`provider`]: the [`Provider`] impl — request mapping (system prompt, text and tool
//!   messages, JSON-Schema tools, the profile's token-cap field), response mapping
//!   (message, indexed tool calls, `finish_reason`, usage), and the transport round trip.
//! - [`sse`]: the SSE parser — `data:` chunks ending `[DONE]`, indexed tool-call deltas
//!   assembled into `spot_llm::StreamEvent`s, usage from the final chunk when present.
//! - [`close`]: the opt-in partial-JSON repair for local models' truncated tool arguments.
//! - [`errors`]: HTTP status + the error body mapped onto the stable `llm.*` codes,
//!   including endpoints that return HTTP 200 with an error object; `retry-after`
//!   honoured; keys and request bodies never quoted.
//! - [`finish_reason`]: `stop` / `length` / `tool_calls` / `content_filter` mapping.
//!
//! The conformance suite (`spot_llm::conformance::run`) runs against this provider on
//! recorded exchanges under `tests/fixtures/` — no network, no real key.

pub mod close;
pub mod errors;
pub mod finish_reason;
pub mod profile;
pub mod provider;
pub mod sse;
#[doc(hidden)]
pub mod test_hooks;
pub mod wire;

pub use errors::WireError;
pub use profile::{AuthStyle, MaxTokensField, Profile};
pub use provider::{ModelInfo, OpenAiProvider};
