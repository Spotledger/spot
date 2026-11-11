//! `spot-llm`: the provider-agnostic model interface (WP-AI-03.T1; blueprint WS-AI, ADR-26).
//!
//! One interface that every model provider implements, so an agent runs unchanged on
//! Anthropic, OpenRouter, Ollama, a local model or any compatible endpoint. Models are
//! bring-your-own-key by default (ADR-26); the provider layer is modelled on the idea of
//! Hermes Agent's provider seam: the same [`request::Request`] in, the same
//! [`response::Response`] or [`stream::StreamEvent`] flow out, whatever answers behind it.
//!
//! The pieces:
//! - [`message`] / [`request`] / [`response`] / [`stream`]: the serde-stable data model —
//!   messages with text, tool-use and tool-result parts; requests with tools, limits and a
//!   caller `request_id`; responses with a [`StopReason`] and token [`Usage`]; stream events
//!   that [`assemble`](stream::assemble) back into the same response.
//! - [`provider::Provider`]: the one trait, object-safe over boxed futures (no `async_trait`).
//! - [`error::LlmError`]: typed, explained errors with stable `llm.*` codes and a retryable
//!   flag (ADR-26: typed, explained errors).
//! - [`ApiKey`]: a key that prints `***` and zeroises on drop.
//! - [`mock::MockProvider`]: the scripted provider tests build on, and
//!   [`conformance`]: the checks every provider must pass, green on the mock and later run
//!   against recorded HTTP exchanges.
//! - [`transport`]: the HTTP seam providers use instead of a concrete client — a trait, a
//!   replaying mock, and the reqwest-backed client behind the `reqwest` feature.
//!
//! Later tasks build on this crate: real providers (T2, T3) and the task router with usage
//! accounting and budgets (T4).

pub mod api_key;
pub mod conformance;
pub mod error;
pub mod message;
pub mod mock;
pub mod provider;
pub mod request;
pub mod response;
pub mod stream;
pub mod transport;

pub use api_key::ApiKey;
pub use error::LlmError;
pub use message::{Message, Part, Role};
pub use mock::{MockAction, MockProvider, MockStep};
pub use provider::{BoxFuture, Capabilities, Provider};
pub use request::{Request, ToolSpec};
pub use response::{Response, StopReason, Usage};
pub use stream::{BoxStream, IterStream, StreamEvent, assemble};
