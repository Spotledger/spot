//! The retriever: [`Retriever::search`] embeds a query, asks the store for candidates, and
//! re-checks every hit against the live permission core before it leaves the crate.
//!
//! The index decides nothing about access. For every candidate chunk the retriever asks the
//! [`ReadCheck`] whether the principal may read the document **now** and whether the principal
//! may read the chunk's fields unmasked; a chunk that fails either check is dropped silently.
//! The result therefore carries no total count, no "hidden" figure and no distinct error for
//! "exists but unreadable" versus "does not exist": a query that matches only unreadable
//! documents returns the same empty [`Hits`] as a query that matches nothing.
//!
//! Scores are relative within one response (a raw cosine similarity), so they reveal nothing
//! about a document the principal cannot see.

use serde::Serialize;
use spot_permissions::Principal;
use spot_storage_contract::{RecordId, Version};

use crate::error::RetrievalError;
use crate::ports::{EmbeddingProvider, FieldView, ReadCheck};
use crate::vector::VectorStore;

/// The default snippet length, in characters.
pub const DEFAULT_SNIPPET_CHARS: usize = 240;
/// The default cap on the query text, in characters.
pub const DEFAULT_MAX_QUERY_CHARS: usize = 4_096;
/// The default cap on how many candidate windows one search examines.
pub const DEFAULT_MAX_WINDOWS: usize = 8;
/// The default cap on how many candidates one search may pull from the store.
pub const DEFAULT_MAX_OVERFETCH: usize = 100;

/// What to look for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The query text.
    pub text: String,
    /// How many visible hits to return.
    pub k: usize,
    /// How many candidates to ask for per visible hit (clamped by
    /// [`RetrieverConfig::max_overfetch`]).
    pub overfetch: usize,
    /// Restrict to these DocTypes; `None` searches every DocType. A DocType the principal
    /// cannot read simply contributes no hits, silently.
    pub doctypes: Option<Vec<String>>,
}

impl Query {
    /// A query for `k` hits over every DocType, with an overfetch of 4.
    #[must_use]
    pub fn new(text: &str, k: usize) -> Self {
        Self {
            text: text.to_owned(),
            k,
            overfetch: 4,
            doctypes: None,
        }
    }
}

/// One visible hit.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The DocType.
    pub doctype: String,
    /// The record id.
    pub id: RecordId,
    /// The version the indexed text came from.
    pub version: Version,
    /// The cosine similarity, relative within this response.
    pub score: f32,
    /// The chunk text, trimmed to the configured length, returned only after both checks.
    pub snippet: String,
}

impl Serialize for Hit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Hit", 5)?;
        state.serialize_field("doctype", &self.doctype)?;
        state.serialize_field("id", &self.id.to_string())?;
        state.serialize_field("version", &self.version.0)?;
        state.serialize_field("score", &self.score)?;
        state.serialize_field("snippet", &self.snippet)?;
        state.end()
    }
}

/// A response: the visible hits, best first, and nothing else.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct Hits {
    /// The visible hits, best first.
    pub hits: Vec<Hit>,
}

impl Hits {
    /// Whether the response is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// How many hits the response carries (a visible count, never a total).
    #[must_use]
    pub fn len(&self) -> usize {
        self.hits.len()
    }
}

/// The limits of one retriever.
#[derive(Clone, Debug)]
pub struct RetrieverConfig {
    /// How many characters of a chunk become its snippet.
    pub snippet_chars: usize,
    /// The longest query text the retriever will embed.
    pub max_query_chars: usize,
    /// The most candidate windows one search examines.
    pub max_windows: usize,
    /// The most candidates one search may pull from the store.
    pub max_overfetch: usize,
}

impl Default for RetrieverConfig {
    fn default() -> Self {
        Self {
            snippet_chars: DEFAULT_SNIPPET_CHARS,
            max_query_chars: DEFAULT_MAX_QUERY_CHARS,
            max_windows: DEFAULT_MAX_WINDOWS,
            max_overfetch: DEFAULT_MAX_OVERFETCH,
        }
    }
}

/// Answers permission-checked queries over a [`VectorStore`].
pub struct Retriever {
    embeddings: std::sync::Arc<dyn EmbeddingProvider>,
    store: std::sync::Arc<dyn VectorStore>,
    read_check: std::sync::Arc<dyn ReadCheck>,
    config: RetrieverConfig,
}

impl Retriever {
    /// A retriever over the injected ports.
    #[must_use]
    pub fn new(
        embeddings: std::sync::Arc<dyn EmbeddingProvider>,
        store: std::sync::Arc<dyn VectorStore>,
        read_check: std::sync::Arc<dyn ReadCheck>,
        config: RetrieverConfig,
    ) -> Self {
        Self {
            embeddings,
            store,
            read_check,
            config,
        }
    }

    /// Searches for `query`, returning at most `query.k` hits the principal may read now.
    ///
    /// # Errors
    ///
    /// [`RetrievalError::QueryTooLong`] when the query text is over the limit,
    /// [`RetrievalError::Embed`] when the provider fails, or [`RetrievalError::Store`] when the
    /// store refuses.
    pub fn search(&self, principal: &Principal, query: &Query) -> Result<Hits, RetrievalError> {
        let chars = query.text.chars().count();
        if chars > self.config.max_query_chars {
            return Err(RetrievalError::QueryTooLong { chars });
        }
        let embedded = self.embeddings.embed(std::slice::from_ref(&query.text))?;
        let vector = embedded.first().ok_or_else(|| RetrievalError::Embed {
            detail: "the provider returned no vector for the query".to_owned(),
        })?;

        let k = query.k.max(1);
        let overfetch = query.overfetch.max(1);
        let step = k.saturating_mul(overfetch);
        let mut want = step.min(self.config.max_overfetch).max(1);
        let mut offset = 0usize;
        let mut windows = 0usize;
        let mut hits: Vec<Hit> = Vec::new();
        loop {
            windows += 1;
            let candidates = self.store.search(vector, want)?;
            if candidates.len() <= offset {
                break;
            }
            for candidate in &candidates[offset..] {
                if hits.len() >= k {
                    break;
                }
                let chunk = &candidate.chunk;
                if !self.doctype_allowed(query, &chunk.doctype) {
                    continue;
                }
                if !self
                    .read_check
                    .can_read(principal, &chunk.doctype, chunk.id)
                    .allowed
                {
                    continue;
                }
                let view: FieldView = self.read_check.readable_fields(principal, &chunk.doctype);
                if chunk.fields.iter().any(|field| !view.allows(field)) {
                    continue;
                }
                hits.push(Hit {
                    doctype: chunk.doctype.clone(),
                    id: chunk.id,
                    version: chunk.version,
                    score: candidate.score,
                    snippet: snippet(&chunk.text, self.config.snippet_chars),
                });
            }
            offset = candidates.len();
            if hits.len() >= k || windows >= self.config.max_windows || candidates.len() < want {
                break;
            }
            want = want.saturating_add(step).min(self.config.max_overfetch);
        }
        Ok(Hits { hits })
    }

    /// Whether the query's DocType filter admits `doctype`.
    fn doctype_allowed(&self, query: &Query, doctype: &str) -> bool {
        query
            .doctypes
            .as_ref()
            .is_none_or(|list| list.iter().any(|d| d == doctype))
    }
}

/// The first `chars` characters of `text`, trimmed at the end.
fn snippet(text: &str, chars: usize) -> String {
    text.chars()
        .take(chars)
        .collect::<String>()
        .trim_end()
        .to_owned()
}
