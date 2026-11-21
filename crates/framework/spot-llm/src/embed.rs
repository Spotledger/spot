//! Embeddings next to completions (WP-AI-03.T6; blueprint WS-AI, ADR-26).
//!
//! [`Embedder`] is a trait of its own, not a method on [`Provider`](crate::Provider): a
//! provider that does not embed simply does not implement it, and the existing `Provider`
//! implementors and the conformance suite stay untouched. One [`EmbedRequest`] in, one
//! [`EmbedResponse`] out, the same typed [`LlmError`]s as a completion.
//!
//! The data model is bounded before anything is sent ([`EmbedRequest::validate`]) and what
//! comes back is checked before anything is passed on ([`EmbedResponse::validate_against`]):
//! one vector per input in order, none empty, every value finite, one width throughout.
//! Neither type prints an input text, a vector or a key in `Debug`; errors name counts and
//! limits only.

use std::sync::Arc;

use crate::error::LlmError;
use crate::provider::BoxFuture;
use crate::response::Usage;

/// The most inputs one [`EmbedRequest`] may carry.
pub const MAX_EMBED_INPUTS: usize = 256;

/// The longest single input, in bytes.
pub const MAX_EMBED_INPUT_BYTES: usize = 32_768;

/// One embeddings request: a model, the texts to embed and an optional output width.
///
/// `dimensions` asks a model that supports shortened vectors for that width; `None` leaves
/// the model's own width.
#[derive(Clone, PartialEq, Eq)]
pub struct EmbedRequest {
    /// The embedding model id, in the provider's own spelling.
    pub model: String,
    /// The texts to embed; at most [`MAX_EMBED_INPUTS`], each at most
    /// [`MAX_EMBED_INPUT_BYTES`] bytes.
    pub inputs: Vec<String>,
    /// The requested vector width, when the caller wants a specific one.
    pub dimensions: Option<u32>,
}

impl EmbedRequest {
    /// A request for `model` over `inputs`, with the model's own width.
    pub fn new(model: impl Into<String>, inputs: Vec<String>) -> Self {
        Self {
            model: model.into(),
            inputs,
            dimensions: None,
        }
    }

    /// Asks for vectors of `dimensions` values.
    pub fn with_dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    /// Checks the request's bounds. Called by every [`Embedder`] before anything is sent; a
    /// breach is `llm.bad_request` whose detail names the limit and never quotes an input.
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.model.trim().is_empty() {
            return Err(bad_request("the embeddings request names no model"));
        }
        if self.inputs.is_empty() {
            return Err(bad_request("the embeddings request carries no input"));
        }
        if self.inputs.len() > MAX_EMBED_INPUTS {
            return Err(bad_request(format!(
                "the embeddings request carries {} inputs; the limit is {MAX_EMBED_INPUTS}",
                self.inputs.len()
            )));
        }
        if let Some(position) = self
            .inputs
            .iter()
            .position(|input| input.len() > MAX_EMBED_INPUT_BYTES)
        {
            return Err(bad_request(format!(
                "embeddings input {position} is {} bytes; the limit is {MAX_EMBED_INPUT_BYTES}",
                self.inputs[position].len()
            )));
        }
        if self.dimensions == Some(0) {
            return Err(bad_request(
                "the embeddings request asks for zero dimensions",
            ));
        }
        Ok(())
    }
}

impl std::fmt::Debug for EmbedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Counts only: an input text never reaches a log line through `Debug`.
        f.debug_struct("EmbedRequest")
            .field("model", &self.model)
            .field("inputs", &self.inputs.len())
            .field("bytes", &self.inputs.iter().map(String::len).sum::<usize>())
            .field("dimensions", &self.dimensions)
            .finish()
    }
}

/// The reply to one [`EmbedRequest`]: one vector per input, in input order.
#[derive(Clone, PartialEq)]
pub struct EmbedResponse {
    /// The vectors, `vectors[i]` for `inputs[i]`.
    pub vectors: Vec<Vec<f32>>,
    /// The reported usage; only `input_tokens` is meaningful for an embedding.
    pub usage: Usage,
    /// The model that answered, as the provider reports it (the request's when it does not).
    pub model: String,
}

impl EmbedResponse {
    /// Checks the reply against the request it answers: one vector per input, none empty,
    /// no non-finite value, one width throughout. A reply that fails is `llm.protocol`,
    /// never passed on; the detail names counts and positions, never a value.
    pub fn validate_against(&self, request: &EmbedRequest) -> Result<(), LlmError> {
        if self.vectors.len() != request.inputs.len() {
            return Err(protocol(format!(
                "the endpoint returned {} vectors for {} inputs",
                self.vectors.len(),
                request.inputs.len()
            )));
        }
        let mut width: Option<usize> = None;
        for (position, vector) in self.vectors.iter().enumerate() {
            if vector.is_empty() {
                return Err(protocol(format!(
                    "the endpoint returned an empty vector for input {position}"
                )));
            }
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(protocol(format!(
                    "the endpoint returned a non-finite value in the vector for input {position}"
                )));
            }
            match width {
                None => width = Some(vector.len()),
                Some(expected) if expected != vector.len() => {
                    return Err(protocol(format!(
                        "the endpoint returned vectors of different widths ({expected} and {})",
                        vector.len()
                    )));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// The width of the vectors (0 for an empty response).
    pub fn width(&self) -> usize {
        self.vectors.first().map_or(0, Vec::len)
    }
}

impl std::fmt::Debug for EmbedResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedResponse")
            .field("vectors", &self.vectors.len())
            .field("width", &self.width())
            .field("usage", &self.usage)
            .field("model", &self.model)
            .finish()
    }
}

/// A provider that can embed text. Object-safe over boxed futures, like
/// [`Provider`](crate::Provider).
pub trait Embedder: Send + Sync {
    /// The provider's stable id (`openai-compatible`, `anthropic`, `mock-embedder`).
    fn id(&self) -> &str;

    /// Embeds `request.inputs`. Implementations call [`EmbedRequest::validate`] first and
    /// [`EmbedResponse::validate_against`] on what the endpoint returned.
    fn embed<'a>(&'a self, request: EmbedRequest)
    -> BoxFuture<'a, Result<EmbedResponse, LlmError>>;
}

/// Forwards [`Embedder`] through a smart pointer, so `Arc<dyn Embedder>` and
/// `Box<dyn Embedder>` can back a service's embedder field.
macro_rules! forward_embedder {
    ($ptr:ident) => {
        impl<T: Embedder + ?Sized> Embedder for $ptr<T> {
            fn id(&self) -> &str {
                (**self).id()
            }

            fn embed<'a>(
                &'a self,
                request: EmbedRequest,
            ) -> BoxFuture<'a, Result<EmbedResponse, LlmError>> {
                (**self).embed(request)
            }
        }
    };
}

forward_embedder!(Arc);
forward_embedder!(Box);

/// A `llm.bad_request` error with `detail`.
fn bad_request(detail: impl Into<String>) -> LlmError {
    LlmError::BadRequest {
        detail: detail.into(),
    }
}

/// A `llm.protocol` error with `detail`.
fn protocol(detail: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        detail: detail.into(),
    }
}
