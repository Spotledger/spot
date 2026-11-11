#![forbid(unsafe_code)]
//! `spot-retrieval`: permission-aware retrieval over documents (WP-AI-10, blueprint WS13,
//! ADR-070 sections 6 and 12, ADR-25).
//!
//! An agent, a search box or a report finds documents by meaning through a vector index. The
//! index is a **copy** of data, so it is also a place where permissions can silently be lost.
//! This crate makes the rule structural: **the index decides nothing about access**, and every
//! hit is re-checked against the live permission core before it leaves the crate. A restricted
//! principal never sees an unreadable document.
//!
//! # Shape
//!
//! - [`policy::IndexPolicy`]: the TOML index policy — which fields of which DocTypes are
//!   indexed and how their long values are chunked. A DocType with no policy is not indexed
//!   (default deny). A field the permission rules protect is refused unless the policy lists it
//!   under `restricted_ok`, and then its chunks are tagged with the field name.
//! - [`chunk::chunk_text`]: deterministic chunking that splits on paragraph, then sentence,
//!   then a hard boundary, and loses nothing.
//! - [`ports`]: the host-implemented seams — [`ports::EmbeddingProvider`],
//!   [`ports::DocumentSource`], [`ports::PositionStore`], [`ports::VectorStore`] and
//!   [`ports::ReadCheck`]. The host fills `ReadCheck` with the permission engine and a
//!   document-facts source; this crate never evaluates a rule itself.
//! - [`vector::MemoryVectorStore`]: a deterministic in-memory vector store. `upsert` never lets
//!   an older version overwrite a newer one; `search` breaks ties deterministically.
//! - [`indexer::Indexer::pump_once`]: the change-feed indexer, a pure pump the host loop calls,
//!   with a durable position that advances only after the store accepted the batch.
//! - [`retriever::Retriever::search`]: embeds the query, over-fetches candidates, and drops
//!   every candidate the live [`ports::ReadCheck`] refuses — the read right **and** the field
//!   view — before returning at most `k` hits.
//! - [`testing`]: in-memory fakes for every port.
//!
//! # No side channels
//!
//! The result carries no total count and no "hidden" figure, and there is no distinct error for
//! "exists but you may not read it" versus "does not exist": a query that matches only
//! unreadable documents returns the same empty [`retriever::Hits`] as a query that matches
//! nothing. Scores are relative within one response.
//!
//! # Floats, and only here
//!
//! Embeddings are `f32` vectors, so this crate alone may use float arithmetic for vectors and
//! scores; every other type — versions, counts, ids and positions — is integral, and nothing
//! here touches money. The float work is confined to [`vector`], [`retriever`] and
//! [`testing`].
//!
//! # Purity
//!
//! No network, no model, no storage backend, no clock reads (the time is an argument to the
//! indexer) and no spawned tasks.

pub mod chunk;
pub mod error;
pub mod indexer;
pub mod policy;
pub mod ports;
pub mod retriever;
pub mod testing;
pub mod vector;

pub use chunk::chunk_text;
pub use error::{
    CODE_DOCUMENT_TOO_LARGE, CODE_EMBED_FAILED, CODE_FEED_FAILED, CODE_POLICY_FIELD_RESTRICTED,
    CODE_POLICY_INVALID, CODE_POLICY_PARSE, CODE_POLICY_UNKNOWN_DOCTYPE, CODE_POLICY_UNKNOWN_FIELD,
    CODE_POSITION_FAILED, CODE_QUERY_TOO_LONG, CODE_STORE_FAILED, CODES, RetrievalError,
};
pub use indexer::{DEFAULT_MAX_DOCUMENT_CHARS, Indexer, IndexerConfig, PumpReport, Skipped};
pub use policy::{
    Chunking, DEFAULT_MAX_CHUNK_CHARS, DEFAULT_OVERLAP_CHARS, DocTypePolicy, FieldPlan, IndexPolicy,
};
pub use ports::{
    DocumentFields, DocumentSource, EmbeddingProvider, FieldView, PositionStore, ReadCheck,
};
pub use retriever::{
    DEFAULT_MAX_OVERFETCH, DEFAULT_MAX_QUERY_CHARS, DEFAULT_MAX_WINDOWS, DEFAULT_SNIPPET_CHARS,
    Hit, Hits, Query, Retriever, RetrieverConfig,
};
pub use vector::{Chunk, MemoryVectorStore, ScoredChunk, VectorStore, cosine};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-retrieval";
