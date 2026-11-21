//! The ports the host implements and the indexer and retriever read.
//!
//! Every port is a plain synchronous trait (a fake lives in [`crate::testing`]); the only
//! asynchronous dependency is the change feed itself, which the indexer reads through the
//! storage contract's [`ChangeFeed`](spot_storage_contract::ChangeFeed).
//!
//! The ports carry no permission logic: [`ReadCheck`] is the seam the host fills with the
//! permission engine and a document-facts source, and this crate never evaluates a rule itself.

use std::collections::BTreeMap;

use spot_permissions::{Decision, Principal};
use spot_storage_contract::{FeedPosition, RecordId, Version};

use crate::error::RetrievalError;

/// A provider that turns text into `f32` vectors.
///
/// `embed` returns one vector per input, in order. It is the only source of embeddings; the
/// indexer never calls a network or a model itself. `max_batch` bounds how many texts one call
/// carries, so the indexer can honour a provider's limit.
pub trait EmbeddingProvider: Send + Sync {
    /// Embeds `texts`, one vector each, in order.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Embed`] when the provider cannot embed the batch. An error aborts the
    /// indexer run and leaves the feed position unchanged.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, RetrievalError>;

    /// The most texts one `embed` call may carry. The default is 64.
    fn max_batch(&self) -> usize {
        64
    }
}

/// A document's indexable field values at one committed version.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DocumentFields {
    /// The values, by field name, already rendered as text (the host decides how a non-text
    /// value becomes text; this crate only chunks and embeds).
    pub values: BTreeMap<String, String>,
}

/// Loads a document's indexable field values at a committed version.
///
/// `None` means the document is gone at that version; the indexer then removes its chunks.
pub trait DocumentSource: Send + Sync {
    /// The values of `(doctype, id)` at `version`, or `None` when it is gone.
    fn load(&self, doctype: &str, id: RecordId, version: Version) -> Option<DocumentFields>;
}

/// The durable change-feed position of one index.
///
/// `load` returning `None` means the index has never been built; the indexer starts from
/// [`FeedPosition::START`].
pub trait PositionStore: Send + Sync {
    /// The last saved position, or `None` before the first save.
    fn load(&self) -> Option<FeedPosition>;

    /// Makes `position` durable.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Position`] when the store refuses; the indexer then advances nothing.
    fn save(&self, position: FeedPosition) -> Result<(), RetrievalError>;
}

/// The fields of a DocType a principal may read **unmasked**.
///
/// A masked field is deliberately absent: a chunk that carries a masked field's text must never
/// reach a reader who only sees the mask, so the retriever drops it. `All` is the system
/// principal (and any host that decides a field view wholesale).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldView {
    /// Every field is readable unmasked.
    All,
    /// Only these fields are readable unmasked.
    Only(std::collections::BTreeSet<String>),
}

impl FieldView {
    /// Whether `field` may be read unmasked.
    #[must_use]
    pub fn allows(&self, field: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(fields) => fields.contains(field),
        }
    }
}

/// The live read decision for one principal: the seam the host fills with the permission
/// engine and a document-facts source.
///
/// This crate never evaluates a rule itself; it only asks. `can_read` must answer for the
/// document **now**, so a permission revoked after indexing takes effect on the next query
/// without re-indexing.
pub trait ReadCheck: Send + Sync {
    /// Whether `principal` may read `(doctype, id)` now.
    fn can_read(&self, principal: &Principal, doctype: &str, id: RecordId) -> Decision;

    /// The fields of `doctype` `principal` may read unmasked now.
    fn readable_fields(&self, principal: &Principal, doctype: &str) -> FieldView;
}
