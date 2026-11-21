//! `spot-llm-anthropic`: the Anthropic Messages API provider (WP-AI-03.T2; ADR-26).
//!
//! [`AnthropicProvider`] implements [`spot_llm::Provider`] for `/v1/messages` over an injected
//! [`spot_llm::HttpTransport`] — bring-your-own-key ([`spot_llm::ApiKey`]), a configurable base
//! URL (default `https://api.anthropic.com`), and nothing here reads the environment. The model
//! catalogue is data passed in the constructor ([`ModelInfo`]), never hard-coded names with
//! behaviour.
//!
//! The pieces:
//! - [`provider`]: the [`Provider`] impl — request mapping (system prompt, text / tool-use /
//!   tool-result messages, JSON-Schema tools, stop sequences, a required `max_tokens` defaulted
//!   from the model catalogue, temperature as self-formatted decimal text in permille, opt-in
//!   `cache_control` markers), response mapping, and the transport round trip.
//! - [`sse`]: the SSE parser turning Anthropic's event stream into [`spot_llm::StreamEvent`]s,
//!   tolerant of unknown event types; a stream ending without its `message_stop` is
//!   `llm.protocol`.
//! - [`errors`]: HTTP status + Anthropic `error.type` mapped onto the stable `llm.*` codes,
//!   `retry-after` honoured, keys and request bodies never quoted.
//! - [`cache`]: the opt-in prompt-caching marker builder.
//!
//! The conformance suite (`spot_llm::conformance::run`) runs against this provider on recorded
//! exchanges under `tests/fixtures/` — no network, no real key.

pub mod cache;
pub mod errors;
pub mod provider;
pub mod sse;
#[doc(hidden)]
pub mod test_hooks;
pub mod wire;

pub use cache::CacheMarker;
pub use errors::WireError;
pub use provider::{AnthropicOptions, AnthropicProvider, ModelInfo};
