//! Embeddings over the OpenAI-compatible wire (WP-AI-03.T6): `POST {base}/v1/embeddings`.
//!
//! [`OpenAiProvider`] implements [`Embedder`] next to [`spot_llm::Provider`], through the same
//! transport, auth styles and error mapping as a completion ([`crate::errors::map`]: 401 is
//! `llm.auth`, 429 is `llm.rate_limited` with its `retry-after`, 5xx is retryable).
//!
//! The request body is exactly `{"model", "input": [...], "encoding_format": "float"}`, with
//! `dimensions` only when the caller asked. The reply's `data` items are sorted by their
//! `index`; a duplicate or a gap is `llm.protocol`, so is a count that differs from the
//! inputs, an empty vector or a non-finite value ([`EmbedResponse::validate_against`]).
//! A profile with `supports_embeddings = false` is refused with `llm.embeddings_unsupported`
//! before any request is built. No input text, vector value or key appears in an error.

use serde::{Deserialize, Serialize};
use spot_llm::embed::{EmbedRequest, EmbedResponse, Embedder};
use spot_llm::error::LlmError;
use spot_llm::provider::BoxFuture;
use spot_llm::response::Usage;

use crate::errors::map;
use crate::provider::{OpenAiProvider, transport_map};
use crate::wire::ErrorEntry;

/// The wire request body; field order is the serialized order.
#[derive(Serialize)]
struct EmbedBody<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

/// The wire reply.
#[derive(Deserialize)]
struct EmbedReply {
    /// One item per input, in any order.
    #[serde(default)]
    data: Vec<EmbedItem>,
    /// The model that answered.
    #[serde(default)]
    model: Option<String>,
    /// The reported usage.
    #[serde(default)]
    usage: Option<EmbedUsage>,
    /// The endpoint's error object, when the body is an error under a 200 status.
    #[serde(default)]
    error: Option<ErrorEntry>,
}

/// One embedding of the reply.
#[derive(Deserialize)]
struct EmbedItem {
    /// The position of the input this vector belongs to.
    #[serde(default)]
    index: Option<u64>,
    /// The vector (`encoding_format: float`).
    #[serde(default)]
    embedding: Vec<f32>,
}

/// The reply's usage block.
#[derive(Deserialize)]
struct EmbedUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

/// Serializes the request body for `request`.
pub fn request_body(request: &EmbedRequest) -> Result<String, LlmError> {
    let body = EmbedBody {
        model: &request.model,
        input: &request.inputs,
        encoding_format: "float",
        dimensions: request.dimensions,
    };
    serde_json::to_string(&body)
        .map_err(|_| protocol("the embeddings request could not be serialized"))
}

/// Reads the body of a 200 reply for `request`: the vectors in input order, the usage and the
/// answering model, all checked. Public so the recorded-exchange tests can drive it directly.
pub fn parse_reply(request: &EmbedRequest, body: &str) -> Result<EmbedResponse, LlmError> {
    let reply: EmbedReply = serde_json::from_str(body)
        .map_err(|_| protocol("the response body was not the embeddings JSON"))?;
    if reply.error.is_some() {
        // An error object under a 200 status maps like a failed status.
        return Err(map(200, body, None));
    }
    let vectors = ordered_vectors(reply.data)?;
    let response = EmbedResponse {
        vectors,
        usage: Usage {
            input_tokens: reply
                .usage
                .and_then(|usage| usage.prompt_tokens.or(usage.total_tokens))
                .unwrap_or(0),
            ..Usage::ZERO
        },
        model: reply
            .model
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| request.model.clone()),
    };
    response.validate_against(request)?;
    Ok(response)
}

/// Orders the items by their `index`. Items without any index keep wire order; a mix of
/// indexed and unindexed items, a duplicate index or a gap is `llm.protocol`.
fn ordered_vectors(items: Vec<EmbedItem>) -> Result<Vec<Vec<f32>>, LlmError> {
    let indexed = items.iter().filter(|item| item.index.is_some()).count();
    if indexed == 0 {
        return Ok(items.into_iter().map(|item| item.embedding).collect());
    }
    if indexed != items.len() {
        return Err(protocol(
            "the embeddings reply mixes indexed and unindexed items",
        ));
    }
    let mut keyed: Vec<(u64, Vec<f32>)> = items
        .into_iter()
        .filter_map(|item| item.index.map(|index| (index, item.embedding)))
        .collect();
    keyed.sort_by_key(|(index, _)| *index);
    for (position, (index, _)) in keyed.iter().enumerate() {
        if *index != position as u64 {
            return Err(protocol(
                "the embeddings reply has a duplicate or missing index",
            ));
        }
    }
    Ok(keyed.into_iter().map(|(_, vector)| vector).collect())
}

/// A `llm.protocol` error with `detail`.
fn protocol(detail: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        detail: detail.into(),
    }
}

impl Embedder for OpenAiProvider {
    fn id(&self) -> &str {
        "openai-compatible"
    }

    fn embed<'a>(
        &'a self,
        request: EmbedRequest,
    ) -> BoxFuture<'a, Result<EmbedResponse, LlmError>> {
        Box::pin(async move {
            request.validate()?;
            if !self.profile().supports_embeddings {
                return Err(LlmError::EmbeddingsUnsupported);
            }
            let outbound = self.embeddings_outbound(request_body(&request)?)?;
            let http = match self.transport().send(outbound).await {
                Ok(http) => http,
                Err(transport_error) => return Err(transport_map(transport_error)),
            };
            if http.status != 200 {
                return Err(self.error_from(http.status, &http.headers, &http.body));
            }
            parse_reply(&request, &http.body)
        })
    }
}
