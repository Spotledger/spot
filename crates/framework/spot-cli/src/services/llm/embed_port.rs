//! The retrieval service's embedding port, provided by the LLM service (WP-AI-03.T7).
//!
//! [`GatewayEmbedPort`] implements `retrieval::ports::EmbedPort` over [`LlmGateway::embed`], so an
//! embedding asked for by the retrieval indexer or a query passes the allow-list, the egress
//! policy, the budget and the send record of the tenant's provider. It carries no text in what it
//! returns: a failure is [`EmbedFailure`], a stable `llm_host.*` code and nothing else.
//!
//! - `request.provider` is the LLM service tenant that embeds (`[services.retrieval] provider`
//!   names it; the tenant's `embedding_model` and `embedding_dimensions` do the rest).
//! - `request.principal` is `user:<id>` or `agent:<id>` for a query, kept as the audit actor of
//!   the send record; `None` is the indexer, which runs as the named caller `system-indexer`.
//!   Any other principal is refused (`llm_host.config_invalid`) rather than guessed at.
//! - `request.purpose` is the send record's purpose (`retrieval:index`, `retrieval:query`).

use std::sync::Arc;

use super::config::check_tenant;
use super::gateway::{CallError, Caller, LlmGateway};
use crate::services::retrieval::ports::{EmbedFailure, EmbedPort, EmbedRequest, Fut};

/// The caller name of the indexer, which embeds on no user's behalf.
pub const SYSTEM_INDEXER: &str = "system-indexer";

/// [`EmbedPort`] over the LLM gateway.
pub struct GatewayEmbedPort {
    gateway: Arc<LlmGateway>,
}

impl GatewayEmbedPort {
    /// A port over `gateway`.
    #[must_use]
    pub fn new(gateway: Arc<LlmGateway>) -> Self {
        Self { gateway }
    }
}

/// The caller of an embedding request: the user or agent of a query, or the named indexer.
///
/// # Errors
///
/// The stable code `llm_host.config_invalid` when the principal is not `user:<id>` or
/// `agent:<id>`.
fn caller_of(request: &EmbedRequest) -> Result<Caller, EmbedFailure> {
    let user = match request.principal.as_deref() {
        None => SYSTEM_INDEXER.to_owned(),
        Some(principal) => {
            let named = principal
                .strip_prefix("user:")
                .or_else(|| principal.strip_prefix("agent:"));
            match named {
                Some(id) if !id.is_empty() && id.len() <= 200 => principal.to_owned(),
                _ => {
                    return Err(EmbedFailure {
                        code: CallError::Config(super::config::ConfigProblem(
                            "the principal must be user:<id> or agent:<id>",
                        ))
                        .code(),
                    });
                }
            }
        }
    };
    Ok(Caller {
        user,
        purpose: request.purpose.as_str().to_owned(),
    })
}

impl EmbedPort for GatewayEmbedPort {
    fn embed<'a>(
        &'a self,
        request: &'a EmbedRequest,
    ) -> Fut<'a, Result<Vec<Vec<f32>>, EmbedFailure>> {
        Box::pin(async move {
            // A provider name that cannot be a tenant id is a tenant with no embedding setup.
            if check_tenant(&request.provider).is_err() {
                return Err(EmbedFailure {
                    code: CallError::EmbeddingsNotConfigured.code(),
                });
            }
            let caller = caller_of(request)?;
            self.gateway
                .embed(&request.provider, &caller, &request.texts)
                .await
                .map_err(|error| EmbedFailure { code: error.code() })
        })
    }
}
