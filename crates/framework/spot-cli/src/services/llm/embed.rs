//! [`LlmGateway::embed`] (WP-AI-03.T7): an embedding call that passes exactly the controls a
//! completion does. This file is a child module of the gateway, so it reaches the gateway's
//! private tenant stack; its public surface is the one method.
//!
//! The flow, in the order that matters:
//!
//! 1. **The tenant's stack** and its embedding model: a tenant with no `embedding_model` is
//!    refused (`llm_host.embeddings_not_configured`); a stack whose factory built no embedder
//!    answers the provider's fixed `llm.embeddings_unsupported`.
//! 2. **Bounds**: at most `max_texts` texts of at most `max_text_bytes` bytes each
//!    (`llm_host.embed_too_large`).
//! 3. **Protected data never leaves**: every text goes through the redactor; a text it flags as a
//!    secret or a financial identifier is refused, naming only the class
//!    (`llm_host.embed_refused`), and nothing at all is sent.
//! 4. **The allow-list, the key, the egress policy and the budget period** exactly as for a
//!    completion ([`LlmGateway::admit_send`]).
//! 5. **The budget**: the worst case of the whole call (one token per byte, an upper bound
//!    because a token spans at least one byte) is reserved against the tenant's budget at the
//!    tenant's input price before anything is sent; the exact tokens the provider reported are
//!    settled after.
//! 6. **The send record** is written (`sending`) before the first byte leaves: digest, size, the
//!    counts and the purpose. Never a text.
//! 7. **Batches** of at most [`MAX_EMBED_INPUTS`] texts go to the provider; usage is recorded
//!    under the task `embed`, so embedding tokens are counted separately from chat tokens.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use spot_doc_audit::sha256_hex;
use spot_llm::{EmbedRequest, LlmError, MAX_EMBED_INPUTS};
use spot_llm_router::{BudgetContext, DayKey, ReserveOutcome, UsageLedger, UsageRecord, cost};

use super::{CallError, Caller, LlmGateway};
use crate::services::llm::config::check_tenant;
use crate::services::llm::store::SendRecord;

/// The usage task, and the task in the send record, of an embedding call.
pub const EMBED_TASK: &str = "embed";

/// The longest wait before a retry of a retryable provider failure.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);

/// The labels the redactor put in `redacted`, sorted and without repeats. The redactor writes
/// `[redacted:<label>]`; only the label is kept.
fn flagged_classes(redacted: &str) -> BTreeSet<String> {
    let mut classes = BTreeSet::new();
    let mut rest = redacted;
    while let Some(start) = rest.find("[redacted:") {
        let after = &rest[start + "[redacted:".len()..];
        match after.find(']') {
            Some(end) => {
                let label = &after[..end];
                if !label.is_empty() && label.len() <= 40 {
                    classes.insert(label.to_owned());
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    classes
}

impl LlmGateway {
    /// Embeds `texts` for `caller` on `tenant`'s embedding model: one vector per text, in order.
    ///
    /// The call passes the allow-list, the egress policy, the key, the budget and the send record
    /// exactly as [`LlmGateway::complete`] does (see the module notes); `caller.purpose` goes into
    /// the send record, so keep it free of data (`retrieval:index`). An empty `texts` answers
    /// with no vectors and does nothing else.
    ///
    /// # Errors
    ///
    /// A [`CallError`]; every refusal after the tenant's stack was built wrote a send record
    /// first. [`CallError::EmbeddingsNotConfigured`], [`CallError::EmbedTooLarge`],
    /// [`CallError::EmbedRefused`], [`CallError::BudgetExceeded`] and the allow-list, key and
    /// egress refusals send nothing.
    pub async fn embed(
        &self,
        tenant: &str,
        caller: &Caller,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, CallError> {
        check_tenant(tenant)?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let at_ms = self.now_ms();
        let sequence = self.embed_seq.fetch_add(1, Ordering::Relaxed);
        let mut record = SendRecord {
            request_id: format!("{tenant}:{EMBED_TASK}:{at_ms}-{sequence}"),
            tenant: tenant.to_owned(),
            task: EMBED_TASK.to_owned(),
            at_ms,
            principal: caller.user.clone(),
            purpose: caller.purpose.clone(),
            ..SendRecord::default()
        };

        let runtime = match self.runtime_for(tenant).await {
            Ok(runtime) => runtime,
            Err(error) => {
                record.outcome = error.outcome();
                self.keep(&record).await;
                return Err(error);
            }
        };
        record.endpoint = runtime.origin.clone();
        record.provider = runtime.config.kind.provider_id().to_owned();
        let Some(model) = runtime.config.embedding_model.clone() else {
            record.outcome = CallError::EmbeddingsNotConfigured.outcome();
            self.keep(&record).await;
            return Err(CallError::EmbeddingsNotConfigured);
        };
        record.model = model.clone();
        let total_bytes: u64 = texts.iter().map(|text| text.len() as u64).sum();
        record.size_bytes = total_bytes;
        record
            .classes
            .insert("texts".to_owned(), texts.len() as u64);

        // The embedder is built per stack; none means the factory cannot embed.
        let Some(embedder) = runtime.embedder.clone() else {
            let error = CallError::Provider(LlmError::EmbeddingsUnsupported);
            record.outcome = error.outcome();
            self.keep(&record).await;
            return Err(error);
        };

        let limits = self.embed_limits;
        if texts.len() > limits.max_texts || texts.iter().any(|t| t.len() > limits.max_text_bytes) {
            let error = CallError::EmbedTooLarge;
            record.outcome = error.outcome();
            self.keep(&record).await;
            return Err(error);
        }

        // Protected data never leaves: refuse (not redact) what the redactor flags, and send
        // the whole call as it is or not at all.
        let mut classes = BTreeSet::new();
        let mut flagged_texts: u64 = 0;
        for text in texts {
            let (redacted, count) = self.deps.redactor.redact_str_counted(text);
            if count > 0 {
                flagged_texts += 1;
                classes.extend(flagged_classes(&redacted));
            }
        }
        if flagged_texts > 0 {
            record
                .classes
                .insert("refused_texts".to_owned(), flagged_texts);
            let error = CallError::EmbedRefused {
                classes: classes.into_iter().collect(),
            };
            record.outcome = error.outcome();
            self.keep(&record).await;
            return Err(error);
        }

        // The digest covers the model, the width asked for and the texts, in order. Only the
        // digest and the size are kept.
        let digest_input =
            serde_json::to_vec(&(&model, runtime.config.embedding_dimensions, texts)).map_err(
                |_| {
                    CallError::Provider(LlmError::BadRequest {
                        detail: "the request could not be encoded".to_owned(),
                    })
                },
            );
        match digest_input {
            Ok(bytes) => record.digest = sha256_hex(&bytes),
            Err(error) => {
                record.outcome = "failed_encode".to_owned();
                self.keep(&record).await;
                return Err(error);
            }
        }

        if let Err(error) = self.admit_send(&runtime, tenant, caller).await {
            record.outcome = error.outcome();
            self.keep(&record).await;
            return Err(error);
        }

        // The worst case of the whole call, reserved before anything is sent.
        let context = BudgetContext::new(tenant, EMBED_TASK, DayKey::from_millis(at_ms));
        let estimate = cost(total_bytes, 0, &runtime.price);
        match runtime.router.budgets().reserve(&context, estimate) {
            ReserveOutcome::Reserved { warnings } => {
                for warning in warnings {
                    tracing::warn!(
                        tenant = %tenant,
                        percent = warning.percent,
                        "llm: a tenant's budget crossed its warning level"
                    );
                }
            }
            ReserveOutcome::Exhausted(refusal) => {
                let error = CallError::BudgetExceeded {
                    spent_micro: u64::try_from(refusal.spent_micro()).unwrap_or(u64::MAX),
                    limit_micro: refusal.limit_micro(),
                };
                record.outcome = error.outcome();
                self.keep(&record).await;
                return Err(error);
            }
        }

        // From here bytes may leave: the record exists before they do.
        record.outcome = "sending".to_owned();
        if let Err(error) = self.deps.store.put_send(&record).await {
            runtime.router.budgets().release(&context, estimate);
            return Err(error.into());
        }

        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        let mut tokens: u64 = 0;
        let mut failure: Option<CallError> = None;
        'batches: for batch in texts.chunks(MAX_EMBED_INPUTS) {
            let mut attempt: u32 = 0;
            let reply = loop {
                let mut request = EmbedRequest::new(model.clone(), batch.to_vec());
                request.dimensions = runtime.config.embedding_dimensions;
                match embedder.embed(request).await {
                    Ok(reply) => break reply,
                    Err(error) if error.retryable() && attempt < self.deps.max_retries => {
                        attempt += 1;
                        let wait = error
                            .retry_after()
                            .unwrap_or(Duration::from_secs(1))
                            .min(MAX_RETRY_WAIT);
                        tokio::time::sleep(wait).await;
                    }
                    Err(error) => {
                        failure = Some(CallError::Provider(error));
                        break 'batches;
                    }
                }
            };
            // The width asked for is the width received: a model that ignores `dimensions`
            // must not feed a table fixed at another width.
            if let Some(wanted) = runtime.config.embedding_dimensions
                && reply.vectors.iter().any(|v| v.len() != wanted as usize)
            {
                failure = Some(CallError::Provider(LlmError::Protocol {
                    detail: "the embedding width differs from the configured dimensions".to_owned(),
                }));
                break 'batches;
            }
            // A provider that reports no tokens is counted at one per four bytes, so a call is
            // never free.
            let batch_bytes: u64 = batch.iter().map(|text| text.len() as u64).sum();
            tokens = tokens.saturating_add(if reply.usage.input_tokens > 0 {
                reply.usage.input_tokens
            } else {
                batch_bytes.div_ceil(4)
            });
            vectors.extend(reply.vectors);
        }

        // Settle what was actually used (nothing if the first batch failed), count it as
        // `embed` usage and close the record.
        let actual = cost(tokens, 0, &runtime.price);
        runtime.router.budgets().settle(&context, estimate, actual);
        if tokens > 0 {
            runtime.ledger.record(UsageRecord {
                at: at_ms,
                tenant: tenant.to_owned(),
                task: EMBED_TASK.to_owned(),
                provider: runtime.config.kind.provider_id().to_owned(),
                model: model.clone(),
                input: tokens,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                request_id: record.request_id.clone(),
            });
        }
        record.input_tokens = tokens;
        record.outcome = match &failure {
            None => "sent".to_owned(),
            Some(error) => error.outcome(),
        };
        self.keep(&record).await;
        self.flush(&runtime).await;
        match failure {
            None => Ok(vectors),
            Some(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::flagged_classes;

    #[test]
    fn only_the_labels_are_kept() {
        let classes =
            flagged_classes("a [redacted:card_number] b [redacted:token] [redacted:card_number]");
        assert_eq!(
            classes.into_iter().collect::<Vec<_>>(),
            vec!["card_number".to_owned(), "token".to_owned()]
        );
        assert!(flagged_classes("nothing here [redacted:").is_empty());
        assert!(flagged_classes("").is_empty());
    }
}
