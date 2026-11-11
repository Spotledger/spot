//! The change-feed indexer: [`Indexer::pump_once`] turns committed document changes into
//! chunks in a [`VectorStore`], from a durable [`FeedPosition`].
//!
//! One indexer serves one site, exactly as the realtime feed pump does. It follows the same
//! shape: a pure `pump_once` the host loop calls, a durable position, committed entries only,
//! no spawned tasks. The change feed holds one net entry per document per committed
//! transaction, so the indexer sees each document's final committed version once.
//!
//! For an insert or update the indexer loads the document at its committed version, chunks
//! every policy field, embeds the chunk texts in bounded batches and replaces the document's
//! chunks; for a delete it removes them. The stored position advances **only after** the store
//! accepted the batch, so a provider or store failure leaves the position where it was and the
//! next call retries. A document that is too large to embed is recorded in
//! [`PumpReport::skipped`] and never stops the feed. Replaying a range changes nothing: the
//! store is idempotent by `(doctype, id, version, chunk_no)` and an older version never
//! overwrites a newer one.

use std::sync::Arc;
use std::time::SystemTime;

use spot_core::SiteId;
use spot_storage_contract::{Change, ChangeFeed, ChangeOp, FeedPosition, RecordId, Version};

use crate::chunk::chunk_text;
use crate::error::{CODE_DOCUMENT_TOO_LARGE, CODE_EMBED_FAILED, RetrievalError};
use crate::policy::{DocTypePolicy, IndexPolicy};
use crate::ports::{DocumentSource, EmbeddingProvider, PositionStore};
use crate::vector::{Chunk, VectorStore};

/// The default cap on a document's total indexable characters.
pub const DEFAULT_MAX_DOCUMENT_CHARS: usize = 1_000_000;

/// The limits of one indexer.
#[derive(Clone, Debug)]
pub struct IndexerConfig {
    /// The most characters one document's indexable fields may hold before the document is
    /// skipped as too large.
    pub max_document_chars: usize,
    /// The most chunk texts one embedding call carries; clamped to the provider's
    /// [`max_batch`](EmbeddingProvider::max_batch).
    pub embed_batch: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            max_document_chars: DEFAULT_MAX_DOCUMENT_CHARS,
            embed_batch: 64,
        }
    }
}

/// A document the indexer could not index, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// The DocType.
    pub doctype: String,
    /// The record id.
    pub id: RecordId,
    /// The stable code: `retrieval.document_too_large` or `retrieval.embed_failed`.
    pub code: &'static str,
}

/// One `pump_once` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpReport {
    /// How many feed entries were read.
    pub read: u32,
    /// How many documents were indexed (insert or update).
    pub indexed: u32,
    /// How many documents had their chunks removed (delete, or a document gone at its version).
    pub deleted: u32,
    /// The documents that were skipped, with the reason.
    pub skipped: Vec<Skipped>,
    /// The position as saved after the run (unchanged when the run failed).
    pub position: FeedPosition,
    /// The `now` the caller passed; the indexer never reads the clock itself.
    pub at: SystemTime,
}

/// Indexes one site's change feed into a [`VectorStore`].
pub struct Indexer {
    site: SiteId,
    policy: IndexPolicy,
    documents: Arc<dyn DocumentSource>,
    embeddings: Arc<dyn EmbeddingProvider>,
    store: Arc<dyn VectorStore>,
    positions: Arc<dyn PositionStore>,
    config: IndexerConfig,
}

impl Indexer {
    /// An indexer for `site` over the injected ports.
    #[must_use]
    pub fn new(
        site: SiteId,
        policy: IndexPolicy,
        documents: Arc<dyn DocumentSource>,
        embeddings: Arc<dyn EmbeddingProvider>,
        store: Arc<dyn VectorStore>,
        positions: Arc<dyn PositionStore>,
        config: IndexerConfig,
    ) -> Self {
        Self {
            site,
            policy,
            documents,
            embeddings,
            store,
            positions,
            config,
        }
    }

    /// The site the indexer serves.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The policy the indexer applies.
    #[must_use]
    pub fn policy(&self) -> &IndexPolicy {
        &self.policy
    }

    /// One pump run: read up to `limit` committed changes after the stored position, index
    /// them, and save the new position.
    ///
    /// `now` is the only time the indexer sees; it never reads the clock. A provider, feed or
    /// store failure returns [`RetrievalError`] and leaves the stored position untouched, so
    /// the next call retries the same range (idempotently). A document that is too large, or
    /// whose embedding the provider refuses, is recorded in [`PumpReport::skipped`] and the run
    /// continues.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Feed`], [`RetrievalError::Embed`], [`RetrievalError::Store`] or
    /// [`RetrievalError::Position`].
    pub async fn pump_once<F: ChangeFeed>(
        &mut self,
        feed: &F,
        limit: u32,
        now: SystemTime,
    ) -> Result<PumpReport, RetrievalError> {
        let start = self.positions.load().unwrap_or(FeedPosition::START);
        let changes = feed
            .read_changes(start, limit)
            .await
            .map_err(|e| RetrievalError::Feed {
                detail: e.to_string(),
            })?;
        let mut report = PumpReport {
            read: u32::try_from(changes.len()).unwrap_or(u32::MAX),
            indexed: 0,
            deleted: 0,
            skipped: Vec::new(),
            position: start,
            at: now,
        };
        let mut last = start;
        for change in &changes {
            self.apply(change, &mut report)?;
            last = change.position;
        }
        self.positions.save(last)?;
        report.position = last;
        Ok(report)
    }

    /// Indexes one change. A provider or store failure is returned (the caller leaves the
    /// position alone); a skipped document is recorded and `Ok` is returned.
    fn apply(&self, change: &Change, report: &mut PumpReport) -> Result<(), RetrievalError> {
        let doctype = change.document.entity.as_str();
        let id = change.document.id;
        if change.op == ChangeOp::Delete {
            self.store.delete_document(doctype, id)?;
            report.deleted += 1;
            return Ok(());
        }
        let Some(policy) = self.policy.doctype(doctype) else {
            // Default deny: a DocType with no policy is not indexed. The position still
            // advances past it.
            return Ok(());
        };
        if self.index_document(policy, doctype, id, change.version, report)? {
            report.indexed += 1;
        }
        Ok(())
    }

    /// Indexes one document; `Ok(true)` when chunks were written, `Ok(false)` when it was
    /// skipped or had no indexable text.
    fn index_document(
        &self,
        policy: &DocTypePolicy,
        doctype: &str,
        id: RecordId,
        version: Version,
        report: &mut PumpReport,
    ) -> Result<bool, RetrievalError> {
        let Some(document) = self.documents.load(doctype, id, version) else {
            // Gone at its committed version: leave nothing behind.
            self.store.delete_document(doctype, id)?;
            report.deleted += 1;
            return Ok(false);
        };
        let mut texts: Vec<(String, String)> = Vec::new();
        let mut total = 0usize;
        for plan in &policy.fields {
            let Some(value) = document.values.get(&plan.name) else {
                continue;
            };
            let trimmed = value.trim();
            if trimmed.is_empty() {
                // Never index a value that is empty after trimming.
                continue;
            }
            total += trimmed.chars().count();
            if total > self.config.max_document_chars {
                report.skipped.push(Skipped {
                    doctype: doctype.to_owned(),
                    id,
                    code: CODE_DOCUMENT_TOO_LARGE,
                });
                return Ok(false);
            }
            for text in chunk_text(
                trimmed,
                plan.chunking.max_chunk_chars,
                plan.chunking.overlap_chars,
            ) {
                texts.push((plan.name.clone(), text));
            }
        }
        if texts.is_empty() {
            self.store.delete_document(doctype, id)?;
            return Ok(false);
        }
        let vectors = match self.embed(&texts) {
            Ok(vectors) => vectors,
            Err(Refusal::Provider(error)) => return Err(error),
            Err(Refusal::Refused) => {
                report.skipped.push(Skipped {
                    doctype: doctype.to_owned(),
                    id,
                    code: CODE_EMBED_FAILED,
                });
                return Ok(false);
            }
        };
        let chunks: Vec<Chunk> = texts
            .into_iter()
            .zip(vectors)
            .enumerate()
            .map(|(i, ((field, text), vector))| Chunk {
                doctype: doctype.to_owned(),
                id,
                version,
                chunk_no: u32::try_from(i).unwrap_or(u32::MAX),
                fields: vec![field],
                text,
                vector,
            })
            .collect();
        self.store.upsert(&chunks)?;
        Ok(true)
    }

    /// Embeds every chunk text in bounded batches. A provider error is a run failure; a batch
    /// the provider answers with the wrong shape is a refusal (the document is skipped).
    fn embed(&self, texts: &[(String, String)]) -> Result<Vec<Vec<f32>>, Refusal> {
        let batch = self
            .config
            .embed_batch
            .min(self.embeddings.max_batch())
            .max(1);
        let mut out = Vec::with_capacity(texts.len());
        for window in texts.chunks(batch) {
            let input: Vec<String> = window.iter().map(|(_, text)| text.clone()).collect();
            let embedded = self.embeddings.embed(&input).map_err(Refusal::Provider)?;
            if embedded.len() != input.len() {
                return Err(Refusal::Refused);
            }
            out.extend(embedded);
        }
        Ok(out)
    }
}

/// Why embedding stopped: a provider failure (a run failure) or a refusal (skip the document).
enum Refusal {
    /// The provider returned an error; the run fails.
    Provider(RetrievalError),
    /// The provider answered with the wrong number of vectors; the document is skipped.
    Refused,
}
