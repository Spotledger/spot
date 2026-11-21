//! The agent-memory embedder, provided by the LLM service (WP-AI-03.T7).
//!
//! Agent memory reads `dyn spot_agent_memory::Embedder` (`spot_retrieval::EmbeddingProvider`):
//! synchronous, `embed(&[String])`, `max_batch`. [`GatewayEmbedder`] answers it over
//! [`LlmGateway::embed`], so memory's embeddings pass the same allow-list, budget and send record
//! as everything else.
//!
//! It holds a runtime [`Handle`] captured at `register` and uses [`Handle::block_on`], never
//! `block_in_place`: memory calls it from its own blocking threads (a plain thread or a
//! `spawn_blocking` thread). **Calling it from an async worker thread panics**, as any
//! `block_on` inside a runtime does.
//!
//! The vector width is the embedding tenant's `embedding_dimensions`, and it must equal the width
//! memory's store was prepared with ([`spot_agent_memory::EMBEDDING_DIM`]): [`check_width`] is
//! asked at start and a mismatch is a degraded sentence naming both numbers, not a runtime
//! failure. Every reply is checked again for shape before it is handed on.

use std::sync::Arc;

use spot_llm::MAX_EMBED_INPUTS;
use spot_retrieval::{EmbeddingProvider, RetrievalError};
use tokio::runtime::Handle;

use super::gateway::{CallError, Caller, LlmGateway};

/// The code of a configured embedding width that differs from agent memory's store.
pub const EMBED_DIMENSIONS_MISMATCH: &str = "llm_host.embed_dimensions_mismatch";

/// The caller name of agent memory, which embeds on no user's behalf here.
pub const SYSTEM_MEMORY: &str = "system-agent-memory";

/// The purpose word of the send record.
pub const MEMORY_PURPOSE: &str = "agent-memory";

/// The sentence of a width mismatch at start: it names both numbers.
///
/// # Errors
///
/// The sentence, when `configured` is set and is not `required`. An unset width passes: the
/// reply is checked per call instead.
pub fn check_width(tenant: &str, configured: Option<u32>, required: usize) -> Result<(), String> {
    match configured {
        Some(width) if width as usize != required => Err(format!(
            "{EMBED_DIMENSIONS_MISMATCH}: tenant `{tenant}` embeds {width} values per vector but agent memory's store is {required} wide, so memory cannot use it (set embedding_dimensions = {required})"
        )),
        _ => Ok(()),
    }
}

/// `dyn Embedder` over the LLM gateway.
pub struct GatewayEmbedder {
    gateway: Arc<LlmGateway>,
    /// The tenant whose embedding setup answers; `None` answers `embeddings_not_configured`.
    tenant: Option<String>,
    handle: Handle,
    width: usize,
}

impl GatewayEmbedder {
    /// An embedder for `tenant` (or none) answering vectors `width` wide, blocking on `handle`.
    #[must_use]
    pub fn new(
        gateway: Arc<LlmGateway>,
        tenant: Option<String>,
        width: usize,
        handle: Handle,
    ) -> Self {
        Self {
            gateway,
            tenant,
            handle,
            width,
        }
    }
}

fn refused(code: &str) -> RetrievalError {
    RetrievalError::Embed {
        detail: format!("the embedding provider refused ({code})"),
    }
}

impl EmbeddingProvider for GatewayEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, RetrievalError> {
        let Some(tenant) = self.tenant.as_deref() else {
            return Err(refused(CallError::EmbeddingsNotConfigured.code()));
        };
        let caller = Caller {
            user: SYSTEM_MEMORY.to_owned(),
            purpose: MEMORY_PURPOSE.to_owned(),
        };
        let vectors = self
            .handle
            .block_on(self.gateway.embed(tenant, &caller, texts))
            .map_err(|error| refused(error.code()))?;
        let well_shaped = vectors.len() == texts.len()
            && vectors
                .iter()
                .all(|v| v.len() == self.width && v.iter().all(|x| x.is_finite()));
        if !well_shaped {
            return Err(RetrievalError::Embed {
                detail: "the embedding provider returned vectors of the wrong shape".to_owned(),
            });
        }
        Ok(vectors)
    }

    fn max_batch(&self) -> usize {
        MAX_EMBED_INPUTS
    }
}
