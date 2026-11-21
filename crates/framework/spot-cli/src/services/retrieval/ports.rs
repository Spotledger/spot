//! The seams the retrieval service takes, and the adapters that turn them into the synchronous
//! traits `spot-retrieval` is written against.
//!
//! `spot-retrieval` is plain synchronous Rust (`EmbeddingProvider`, `DocumentSource`,
//! `ReadCheck`, `PositionStore`); the site's services are asynchronous. The adapters here bridge
//! the two: from a runtime worker thread they use `block_in_place`, from a blocking-pool thread
//! they block directly, both on the site's runtime handle. **The site's runtime must be the
//! multi-thread flavour** (`spot serve` builds one); a current-thread runtime panics in
//! `block_in_place`, which the tests of this module avoid by saying `flavor = "multi_thread"`.
//!
//! # The three seams of this module
//!
//! - [`EmbedPort`]: turns texts into vectors under the tenant's provider policy. The **LLM
//!   service** (WP-AI-03.T5) is where an embedding call must go so that its allow-list, budget and
//!   send record apply; it provides this port. Nothing in this module calls a model, a URL or a
//!   socket, and no fake embedder exists outside the tests: without the port the retrieval
//!   service is *degraded*, never silently running on a stand-in.
//! - [`Access`]: whether the **calling user** may read one document now. The host answers it with
//!   a permission-checked load as that user ([`crate::services::retrieval::access::LoaderAccess`]).
//! - [`GuardSource`]: the field guard of the metadata served now (channel `Search`).
//!
//! The indexer reads documents through the site's `SystemReader` port and the change feed through
//! the backend; both exist already.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_app_api::context::Actor;
use spot_app_api::prelude::RecordId;
use spot_field_guard::FieldGuard;
use spot_permissions::{Decision, Principal};
use spot_retrieval::{
    DocumentFields, DocumentSource, EmbeddingProvider, FieldView, PositionStore, ReadCheck,
    RetrievalError,
};
use spot_storage_contract::{FeedPosition, Value, Version};
use tokio::runtime::Handle;

use crate::services::ports::SystemReader;

/// A boxed future the seams return.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why an embedding is being asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedPurpose {
    /// Chunks of an indexed document, asked for by the indexer (no calling user).
    Index,
    /// The text of a query, asked for on behalf of a calling user.
    Query,
}

impl EmbedPurpose {
    /// The word the send record uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Index => "retrieval:index",
            Self::Query => "retrieval:query",
        }
    }
}

/// One embedding request.
#[derive(Clone, Debug)]
pub struct EmbedRequest {
    /// The configured provider name (`[services.retrieval] provider`), resolved by the port.
    pub provider: String,
    /// Why the texts are embedded.
    pub purpose: EmbedPurpose,
    /// The audit actor (`user:<id>` or `agent:<id>`) of a query; `None` for the indexer.
    pub principal: Option<String>,
    /// The texts, in order. Protected fields never reach here: the indexer keeps them out.
    pub texts: Vec<String>,
}

/// A failed embedding: a stable code and nothing else. A provider's message can quote the text it
/// was sent, so it never travels through this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbedFailure {
    /// A stable code such as `llm.budget_exceeded` or `llm.endpoint_refused`.
    pub code: &'static str,
}

/// Turns texts into vectors under the tenant's provider policy. Provided by the LLM service.
pub trait EmbedPort: Send + Sync {
    /// One vector per text, in order.
    fn embed<'a>(
        &'a self,
        request: &'a EmbedRequest,
    ) -> Fut<'a, Result<Vec<Vec<f32>>, EmbedFailure>>;
}

/// Whether the calling user may read one document now. A host answers `false` for anything it
/// cannot decide: the answer fails closed.
pub trait Access: Send + Sync {
    /// Whether `actor` may read `(doctype, id)` at this moment.
    fn can_read<'a>(&'a self, actor: &'a Actor, doctype: &'a str, id: RecordId) -> Fut<'a, bool>;
}

/// The field guard over the metadata served now, or `None` when nothing is served.
pub trait GuardSource: Send + Sync {
    /// A guard that is authoritative for channel `Search`, built from the live metadata.
    fn guard(&self) -> Option<Arc<FieldGuard>>;
}

/// Blocks on `future` from a thread that may not be async: a runtime worker (`block_in_place`) or
/// any other thread.
pub fn block<T>(handle: &Handle, future: impl Future<Output = T>) -> T {
    if Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        handle.block_on(future)
    }
}

/// [`EmbeddingProvider`] over an [`EmbedPort`], with the shape checks the crate relies on.
pub struct BlockingEmbedder {
    port: Arc<dyn EmbedPort>,
    provider: String,
    purpose: EmbedPurpose,
    principal: Option<String>,
    dim: usize,
    batch: usize,
    handle: Handle,
}

impl BlockingEmbedder {
    /// An embedder for `purpose`, expecting vectors `dim` wide and at most `batch` texts a call.
    #[must_use]
    pub fn new(
        port: Arc<dyn EmbedPort>,
        provider: &str,
        purpose: EmbedPurpose,
        principal: Option<String>,
        dim: usize,
        batch: usize,
        handle: Handle,
    ) -> Self {
        Self {
            port,
            provider: provider.to_owned(),
            purpose,
            principal,
            dim,
            batch: batch.max(1),
            handle,
        }
    }
}

impl EmbeddingProvider for BlockingEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, RetrievalError> {
        let request = EmbedRequest {
            provider: self.provider.clone(),
            purpose: self.purpose,
            principal: self.principal.clone(),
            texts: texts.to_vec(),
        };
        let vectors = block(&self.handle, self.port.embed(&request)).map_err(|failure| {
            RetrievalError::Embed {
                detail: format!("the embedding provider refused ({})", failure.code),
            }
        })?;
        let well_shaped = vectors.len() == texts.len()
            && vectors
                .iter()
                .all(|v| v.len() == self.dim && v.iter().all(|x| x.is_finite()));
        if !well_shaped {
            return Err(RetrievalError::Embed {
                detail: "the embedding provider returned vectors of the wrong shape".to_owned(),
            });
        }
        Ok(vectors)
    }

    fn max_batch(&self) -> usize {
        self.batch
    }
}

/// [`ReadCheck`] for one caller: the read right is [`Access::can_read`] for the caller's actor;
/// the field view is [`FieldView::All`] **because the field guard is authoritative** (the
/// retriever is built with `with_search_guard`, and the tool refuses to run without a guard).
pub struct CallerRead {
    access: Arc<dyn Access>,
    actor: Actor,
    handle: Handle,
}

impl CallerRead {
    /// A read check that asks `access` as `actor`.
    #[must_use]
    pub fn new(access: Arc<dyn Access>, actor: Actor, handle: Handle) -> Self {
        Self {
            access,
            actor,
            handle,
        }
    }
}

impl ReadCheck for CallerRead {
    fn can_read(&self, _principal: &Principal, doctype: &str, id: RecordId) -> Decision {
        let allowed = block(&self.handle, self.access.can_read(&self.actor, doctype, id));
        Decision {
            allowed,
            reasons: Vec::new(),
        }
    }

    fn readable_fields(&self, _principal: &Principal, _doctype: &str) -> FieldView {
        FieldView::All
    }
}

/// [`DocumentSource`] over the site's [`SystemReader`]: the indexer's reads, as the platform
/// (there is no calling user on the indexing path; the permission check happens at query time).
///
/// Only text values are indexed; every other type is skipped (the host decides how a value
/// becomes text, and a number or a date is not something to search by meaning).
///
/// The trait answers `Option`, and `None` means "gone": a reader **failure** must never read as
/// "gone" and delete a document's chunks without a trace. A failure therefore sets `failed`, which
/// [`GatedPositions`] turns into a refusal to advance the cursor, so the range is retried.
pub struct SystemDocuments {
    reader: Arc<dyn SystemReader>,
    handle: Handle,
    failed: Arc<AtomicBool>,
}

impl SystemDocuments {
    /// A source reading through `reader`, recording failures in `failed`.
    #[must_use]
    pub fn new(reader: Arc<dyn SystemReader>, handle: Handle, failed: Arc<AtomicBool>) -> Self {
        Self {
            reader,
            handle,
            failed,
        }
    }
}

impl DocumentSource for SystemDocuments {
    fn load(&self, doctype: &str, id: RecordId, version: Version) -> Option<DocumentFields> {
        let name = id.to_string();
        match block(&self.handle, self.reader.read(doctype, &name)) {
            Ok(Some(document)) => {
                if document.version().is_some_and(|stored| stored < version) {
                    // Older than the change that named it: the read is not yet what the feed says.
                    self.failed.store(true, Ordering::SeqCst);
                    return None;
                }
                let mut fields = DocumentFields::default();
                for (field, value) in document.fields() {
                    if let Value::Text(text) = value {
                        fields
                            .values
                            .insert(field.as_str().to_owned(), text.clone());
                    }
                }
                Some(fields)
            }
            Ok(None) => None,
            Err(_) => {
                self.failed.store(true, Ordering::SeqCst);
                None
            }
        }
    }
}

/// A [`PositionStore`] that refuses to advance after a document read failed.
pub struct GatedPositions {
    inner: Arc<dyn PositionStore>,
    failed: Arc<AtomicBool>,
}

impl GatedPositions {
    /// Gates `inner` on `failed`.
    #[must_use]
    pub fn new(inner: Arc<dyn PositionStore>, failed: Arc<AtomicBool>) -> Self {
        Self { inner, failed }
    }
}

impl PositionStore for GatedPositions {
    fn load(&self) -> Option<FeedPosition> {
        self.inner.load()
    }

    fn save(&self, position: FeedPosition) -> Result<(), RetrievalError> {
        if self.failed.swap(false, Ordering::SeqCst) {
            return Err(RetrievalError::Position {
                detail: "a document could not be read; the position stays so the range is retried"
                    .to_owned(),
            });
        }
        self.inner.save(position)
    }
}

/// The durable cursor: one small file per site, `sites/<site>/retrieval.cursor`, holding
/// `<generation> <sequence>`. It is written to a temporary file and renamed, so a crash leaves the
/// old position or the new one, never half of one. The file is node-local: a node that takes the
/// indexer lease over starts from its own last position (or from the start), and because the
/// store replaces by `(document, version)` a replay from an older position changes nothing.
pub struct FilePositions {
    path: PathBuf,
}

impl FilePositions {
    /// A cursor in `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Resets the cursor to the start of the feed (the backfill).
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Position`] when the file cannot be written.
    pub fn reset(&self) -> Result<(), RetrievalError> {
        self.save(FeedPosition::START)
    }
}

impl PositionStore for FilePositions {
    fn load(&self) -> Option<FeedPosition> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let mut parts = text.trim().split(' ').filter_map(|p| p.parse::<u64>().ok());
        match (parts.next(), parts.next()) {
            (Some(generation), Some(seq)) => Some(FeedPosition::new(generation, seq)),
            _ => None,
        }
    }

    fn save(&self, position: FeedPosition) -> Result<(), RetrievalError> {
        let failed = || RetrievalError::Position {
            detail: "the retrieval cursor file could not be written".to_owned(),
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| failed())?;
        }
        let temporary = self.path.with_extension("cursor.tmp");
        std::fs::write(
            &temporary,
            format!("{} {}", position.generation(), position.seq()),
        )
        .map_err(|_| failed())?;
        std::fs::rename(&temporary, &self.path).map_err(|_| failed())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn temp_cursor(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("spot_retrieval_{name}_{}", RecordId::new_v7()))
    }

    #[test]
    fn the_cursor_round_trips_and_resets() {
        let positions = FilePositions::new(temp_cursor("cursor").join("retrieval.cursor"));
        assert_eq!(positions.load(), None);
        positions.save(FeedPosition::new(3, 41)).unwrap();
        assert_eq!(positions.load(), Some(FeedPosition::new(3, 41)));
        positions.reset().unwrap();
        assert_eq!(positions.load(), Some(FeedPosition::START));
    }

    #[test]
    fn a_failed_read_keeps_the_position_once_and_then_lets_it_move() {
        let failed = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(FilePositions::new(
            temp_cursor("gate").join("retrieval.cursor"),
        ));
        let gated =
            GatedPositions::new(Arc::clone(&inner) as Arc<dyn PositionStore>, failed.clone());
        failed.store(true, Ordering::SeqCst);
        assert!(gated.save(FeedPosition::new(1, 5)).is_err());
        assert_eq!(inner.load(), None, "the position did not move");
        gated.save(FeedPosition::new(1, 5)).unwrap();
        assert_eq!(inner.load(), Some(FeedPosition::new(1, 5)));
    }

    #[test]
    fn the_purposes_have_stable_names() {
        assert_eq!(EmbedPurpose::Index.as_str(), "retrieval:index");
        assert_eq!(EmbedPurpose::Query.as_str(), "retrieval:query");
    }
}
