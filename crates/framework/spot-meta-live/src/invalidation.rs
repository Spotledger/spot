//! What travels on the [`InvalidationBus`](crate::InvalidationBus): a metadata version and hash,
//! plus an open kind for the cache keys WP-PLT-19 will invalidate.

use crate::snapshot::{MetaHash, Version};

/// The kind of change an [`Invalidation`] announces.
///
/// `#[non_exhaustive]`: WP-PLT-19 adds the cache-key kinds it needs; callers must keep a
/// catch-all arm.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidationKind {
    /// The site's metadata snapshot changed to `version` with content `hash`.
    Meta,
    /// Cache entries under these keys are stale and must be dropped (WP-PLT-19). Unused by the
    /// metadata layer; carried here so the bus has one vocabulary.
    Cache {
        /// The keys to invalidate.
        keys: Vec<String>,
    },
}

/// One bus message: the kind, the version it refers to and the content hash.
///
/// The same struct carries a metadata change and a future cache-key change; `kind` says which,
/// and the `version`/`hash` fields are the metadata's identity (`0`/empty for a pure cache
/// message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalidation {
    /// What changed.
    pub kind: InvalidationKind,
    /// The metadata version the message refers to.
    pub version: Version,
    /// The metadata content hash the message refers to.
    pub hash: MetaHash,
}

impl Invalidation {
    /// A metadata change to `version` with content `hash`.
    #[must_use]
    pub fn meta(version: Version, hash: MetaHash) -> Self {
        Self {
            kind: InvalidationKind::Meta,
            version,
            hash,
        }
    }

    /// A cache-key invalidation (WP-PLT-19).
    #[must_use]
    pub fn cache(version: Version, hash: MetaHash, keys: Vec<String>) -> Self {
        Self {
            kind: InvalidationKind::Cache { keys },
            version,
            hash,
        }
    }

    /// Whether this is a metadata change.
    #[must_use]
    pub fn is_meta(&self) -> bool {
        self.kind == InvalidationKind::Meta
    }
}

/// One item a bus subscription yields.
///
/// `#[non_exhaustive]`: new bus vocabulary may be added.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusEvent {
    /// A change was published for the subscribed site.
    Change(Invalidation),
    /// The subscriber fell behind and missed changes: the host must reconcile from the version
    /// store (see [`Follower::poll_once`](crate::Follower::poll_once)) rather than trust the
    /// stream to be gap-free.
    Gap,
}
