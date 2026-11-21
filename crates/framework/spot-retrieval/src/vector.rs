//! The vector store: chunks, the store trait, and the deterministic in-memory implementation.
//!
//! A chunk is one window of one field of one document at one version. The store keys it by
//! `(doctype, id, chunk_no)` and never lets an older version overwrite a newer one: a replayed
//! or out-of-order change is a no-op. Nothing on a chunk is a permission: the only rights data
//! is the list of field names the chunk's text came from, which the retriever checks against
//! the live field view.
//!
//! Similarity is cosine over `f32` vectors. This is the one place the crate does float
//! arithmetic (the crate docs say so); every other type — versions, counts, ids, positions —
//! stays integral.

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_storage_contract::{RecordId, Version};

use crate::error::RetrievalError;

/// One indexed window of a document.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// The DocType.
    pub doctype: String,
    /// The record id.
    pub id: RecordId,
    /// The committed version the text came from.
    pub version: Version,
    /// The window's number within the document's version, from zero.
    pub chunk_no: u32,
    /// The field names the text came from (one per chunk today); the retriever's field check.
    pub fields: Vec<String>,
    /// The window's text.
    pub text: String,
    /// The embedding of `text`.
    pub vector: Vec<f32>,
}

/// A chunk and its similarity to a query vector.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoredChunk {
    /// The chunk.
    pub chunk: Chunk,
    /// The cosine similarity of `chunk.vector` and the query vector.
    pub score: f32,
}

/// Stores chunks and answers nearest-neighbour queries.
///
/// `upsert` replaces a document's chunks at the incoming version: it drops that document's
/// chunks at or below the incoming version and writes the new ones, and it is a no-op when a
/// newer version is already stored. `delete_document` removes every chunk of a document.
pub trait VectorStore: Send + Sync {
    /// Writes `chunks`, replacing each document's older chunks.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the store refuses; the indexer then leaves the feed
    /// position unchanged.
    fn upsert(&self, chunks: &[Chunk]) -> Result<(), RetrievalError>;

    /// Removes every chunk of `(doctype, id)`.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the store refuses.
    fn delete_document(&self, doctype: &str, id: RecordId) -> Result<(), RetrievalError>;

    /// The `limit` nearest chunks to `vector`, best first.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::Store`] when the store refuses.
    fn search(&self, vector: &[f32], limit: usize) -> Result<Vec<ScoredChunk>, RetrievalError>;
}

/// The store key: one window of one document. The version is the chunk's, checked on write.
type Key = (String, RecordId, u32);

/// A deterministic, in-memory [`VectorStore`]: no backend, no clock, no randomness.
///
/// Ties are broken deterministically: higher score, then DocType, then record id, then chunk
/// number. The same chunks and the same query give the same order every run.
#[derive(Debug, Default)]
pub struct MemoryVectorStore {
    chunks: Mutex<BTreeMap<Key, Chunk>>,
}

impl MemoryVectorStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many chunks the store holds.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// Every chunk of one document, in chunk order.
    #[must_use]
    pub fn chunks_of(&self, doctype: &str, id: RecordId) -> Vec<Chunk> {
        let guard = self.lock();
        let mut out: Vec<Chunk> = guard
            .iter()
            .filter(|((dt, doc, _), _)| dt == doctype && *doc == id)
            .map(|(_, chunk)| chunk.clone())
            .collect();
        out.sort_by_key(|c| c.chunk_no);
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<Key, Chunk>> {
        self.chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl VectorStore for MemoryVectorStore {
    fn upsert(&self, chunks: &[Chunk]) -> Result<(), RetrievalError> {
        let mut guard = self.lock();
        // The highest incoming version per document: only those chunks are written.
        let mut incoming_max: BTreeMap<(String, RecordId), Version> = BTreeMap::new();
        for chunk in chunks {
            incoming_max
                .entry((chunk.doctype.clone(), chunk.id))
                .and_modify(|v| *v = (*v).max(chunk.version))
                .or_insert(chunk.version);
        }
        // The highest stored version per document.
        let mut stored_max: BTreeMap<(String, RecordId), Version> = BTreeMap::new();
        for ((doctype, id, _), chunk) in guard.iter() {
            stored_max
                .entry((doctype.clone(), *id))
                .and_modify(|v| *v = (*v).max(chunk.version))
                .or_insert(chunk.version);
        }
        // Documents to replace: an incoming version at or above what is stored. A lower
        // incoming version never touches a newer one (out-of-order replay is a no-op).
        let mut replace: BTreeMap<(String, RecordId), Version> = BTreeMap::new();
        for (doc, incoming) in &incoming_max {
            if stored_max.get(doc).is_none_or(|stored| incoming >= stored) {
                replace.insert(doc.clone(), *incoming);
            }
        }
        guard.retain(
            |(doctype, id, _), chunk| match replace.get(&(doctype.clone(), *id)) {
                Some(version) => chunk.version > *version,
                None => true,
            },
        );
        for chunk in chunks {
            let doc = (chunk.doctype.clone(), chunk.id);
            if replace.get(&doc) == Some(&chunk.version) {
                guard.insert(
                    (chunk.doctype.clone(), chunk.id, chunk.chunk_no),
                    chunk.clone(),
                );
            }
        }
        Ok(())
    }

    fn delete_document(&self, doctype: &str, id: RecordId) -> Result<(), RetrievalError> {
        self.lock()
            .retain(|(dt, doc, _), _| !(dt == doctype && *doc == id));
        Ok(())
    }

    fn search(&self, vector: &[f32], limit: usize) -> Result<Vec<ScoredChunk>, RetrievalError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut scored: Vec<ScoredChunk> = self
            .lock()
            .values()
            .map(|chunk| ScoredChunk {
                score: cosine(vector, &chunk.vector),
                chunk: chunk.clone(),
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.chunk.doctype.cmp(&b.chunk.doctype))
                .then_with(|| a.chunk.id.cmp(&b.chunk.id))
                .then_with(|| a.chunk.chunk_no.cmp(&b.chunk.chunk_no))
        });
        scored.truncate(limit);
        Ok(scored)
    }
}

/// Cosine similarity of two equal-length vectors; `0.0` when either is zero-length, when the
/// lengths differ, or when either has zero norm.
#[must_use]
#[allow(clippy::float_arithmetic)] // vectors and scores are the crate's one float exception
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}
