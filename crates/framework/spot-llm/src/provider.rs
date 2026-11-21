//! The one trait every model provider implements.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::request::Request;
use crate::response::Response;
use crate::stream::BoxStream;

/// A boxed `Send` future, so [`Provider`] stays object-safe without the `async_trait` crate
/// (WP-AI-03.T1).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a model can do, in the units the platform plans with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Prompt plus output tokens that fit the context window.
    pub context_window: u64,
    /// The largest supported generation.
    pub max_output: u64,
    /// The model accepts tool specifications and answers with tool uses.
    pub tools: bool,
    /// `stream` works for this model.
    pub streaming: bool,
    /// The model accepts images. (This interface has no image part yet; reserved for T2+.)
    pub vision: bool,
    /// The provider offers prompt caching; reported usage may carry cache reads and writes.
    pub prompt_caching: bool,
}

impl Capabilities {
    /// The mock provider's default: text, tools and streaming on; no vision, no caching.
    pub fn mock() -> Self {
        Self {
            context_window: 128_000,
            max_output: 8_192,
            tools: true,
            streaming: true,
            vision: false,
            prompt_caching: false,
        }
    }
}

/// The one interface every provider — Anthropic, OpenRouter, Ollama, local or mock —
/// implements, so an agent runs unchanged on all of them (WP-AI-03.T1).
///
/// Object-safe: `complete` and `stream` return [`BoxFuture`]s rather than `impl Future`, so
/// `&dyn Provider`, `Arc<dyn Provider>` and `Box<dyn Provider>` all work.
pub trait Provider: Send + Sync {
    /// The provider's stable id (`anthropic`, `openrouter`, `ollama`, `mock`).
    fn id(&self) -> &str;

    /// What `model` can do. A model the provider does not know reports the provider's floor.
    fn capabilities(&self, model: &str) -> Capabilities;

    /// Completes the request in one shot.
    fn complete<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, LlmError>>;

    /// Streams the reply as [`StreamEvent`]s. The events for a request assemble — see
    /// [`assemble`](crate::stream::assemble) — into the same response `complete` returns for
    /// it, which is one of the checks [`conformance`](crate::conformance) runs.
    fn stream<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<BoxStream, LlmError>>;
}

/// Forwards [`Provider`] through a smart pointer, so `Arc<P>` and `Box<dyn Provider>` can back
/// an agent's provider field.
macro_rules! forward_provider {
    ($ptr:ident) => {
        impl<T: Provider + ?Sized> Provider for $ptr<T> {
            fn id(&self) -> &str {
                (**self).id()
            }

            fn capabilities(&self, model: &str) -> Capabilities {
                (**self).capabilities(model)
            }

            fn complete<'a>(
                &'a self,
                request: Request,
            ) -> BoxFuture<'a, Result<Response, LlmError>> {
                (**self).complete(request)
            }

            fn stream<'a>(
                &'a self,
                request: Request,
            ) -> BoxFuture<'a, Result<BoxStream, LlmError>> {
                (**self).stream(request)
            }
        }
    };
}

forward_provider!(Arc);
forward_provider!(Box);
