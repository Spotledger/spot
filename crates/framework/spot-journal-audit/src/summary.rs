//! The record summary both sides of an audit see, and the derived entities it skips.
//!
//! A [`RecordSummary`] is the smallest thing that can be compared without reading a record: what
//! it is ([`entity`](RecordSummary::entity), [`id`](RecordSummary::id)), which
//! [`version`](RecordSummary::version) it stands at, and the 32-byte
//! [`content_digest`](RecordSummary::content_digest) of its content. The scratch rebuild and the
//! live database each supply a stream of them (ADR-061 s8), sorted by entity, then id.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

/// The domain prefix of a record digest, so a record's digest can never be confused with a
/// segment's header, block or segment hash (ADR-061 s5), which cover different bytes.
const RECORD_DIGEST_DOMAIN: &[u8] = b"spot.journal.audit.record.v1\0";

/// The digest of one record's canonical content.
///
/// Both sides of an audit must digest a record the same way, so the caller gives the bytes of the
/// same canonical form on both sides (the record image, ADR-061 s3: every declared field and every
/// child row in the native value encoding). The library owns the hash, never the encoding.
#[must_use]
pub fn content_digest(content: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DIGEST_DOMAIN);
    hasher.update(content);
    hasher.finalize().into()
}

/// One record as both sides of a journal audit see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSummary {
    /// The entity (the catalog's storage name).
    pub entity: String,
    /// The record id (a caller-chosen id, ADR-33).
    pub id: String,
    /// The version the record stands at; monotonic per record (ADR-053).
    pub version: u64,
    /// The SHA-256 digest of the record's canonical content, as [`content_digest`] computes it.
    pub content_digest: [u8; 32],
}

impl RecordSummary {
    /// A summary from its parts.
    #[must_use]
    pub fn new(
        entity: impl Into<String>,
        id: impl Into<String>,
        version: u64,
        content_digest: [u8; 32],
    ) -> Self {
        Self {
            entity: entity.into(),
            id: id.into(),
            version,
            content_digest,
        }
    }

    /// A summary whose digest is taken from the record's canonical content.
    #[must_use]
    pub fn from_content(
        entity: impl Into<String>,
        id: impl Into<String>,
        version: u64,
        content: &[u8],
    ) -> Self {
        Self::new(entity, id, version, content_digest(content))
    }

    /// The sort key of a summary: entity, then id, both by byte order. Streams arrive in this
    /// order, and the first differing record is reported in it.
    #[must_use]
    pub fn sort_key(&self) -> (&str, &str) {
        (self.entity.as_str(), self.id.as_str())
    }
}

/// The entities a tenant's catalog flags `derived` (ADR-061 s3): balance tables, caches, search
/// and graph indexes, the analytics mirror.
///
/// The audit skips them, because a rebuild makes projectors fill them instead of replaying them,
/// and lists the ones it actually skipped. The set comes from the catalog, so it is bounded and
/// known before the streams start; it is never built from the records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DerivedEntities {
    names: BTreeSet<String>,
}

impl DerivedEntities {
    /// No entity is derived: every record is compared.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The set of flagged entity names.
    #[must_use]
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Flags one more entity as derived.
    pub fn flag(&mut self, entity: impl Into<String>) {
        self.names.insert(entity.into());
    }

    /// Whether the catalog flags this entity as derived.
    #[must_use]
    pub fn contains(&self, entity: &str) -> bool {
        self.names.contains(entity)
    }

    /// Whether no entity is flagged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}
