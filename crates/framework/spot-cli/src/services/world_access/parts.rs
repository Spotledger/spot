//! The parts of the world: one trait per port of `RemedyCtx`, each implemented by the module
//! that owns the subsystem (files `jobs.rs`, `cache.rs`, `index.rs`, `edge.rs`, `quarantine.rs`,
//! `cases.rs` and `documents.rs` of this directory).
//!
//! A part has two faces:
//!
//! - [`live`](JobsPart::live): the typed port over the real subsystem. A change made through it
//!   is a change to the site.
//! - [`rehearsal`](JobsPart::rehearsal): a **snapshot**. The part reads the live state through its
//!   read methods into the in-memory fixture ops of `spot_remedies::fixture` and hands those out,
//!   so a rehearsal can be changed and thrown away and never writes to the subsystem. A part that
//!   cannot take a snapshot answers [`PartName::rehearsal_unavailable`].
//!
//! The ops are synchronous because the remedy ports are. The composer calls them on a blocking
//! thread, so a part over asynchronous storage blocks on a `tokio::runtime::Handle` it captured
//! when it was built (`Handle::block_on`, never `block_in_place`, which is invalid on a
//! `spawn_blocking` thread); [`capture_runtime`] reads the handle.

use spot_remedies::{CacheOps, CaseOps, DocumentOps, EdgeOps, IndexOps, JobOps, QuarantineOps};

use crate::services::remedies::world::WorldError;

/// The seven parts, by the name the readiness text uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PartName {
    /// Background jobs.
    Jobs,
    /// The cache.
    Cache,
    /// The retrieval index.
    Index,
    /// Edge sync.
    Edge,
    /// Quarantine.
    Quarantine,
    /// BPM cases.
    Cases,
    /// Documents.
    Documents,
}

impl PartName {
    /// Every part, in the order the readiness text lists them.
    pub const ALL: [PartName; 7] = [
        PartName::Jobs,
        PartName::Cache,
        PartName::Index,
        PartName::Edge,
        PartName::Quarantine,
        PartName::Cases,
        PartName::Documents,
    ];

    /// The stable name (`jobs`, `cache`, `index`, `edge`, `quarantine`, `cases`, `documents`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PartName::Jobs => "jobs",
            PartName::Cache => "cache",
            PartName::Index => "index",
            PartName::Edge => "edge",
            PartName::Quarantine => "quarantine",
            PartName::Cases => "cases",
            PartName::Documents => "documents",
        }
    }

    /// The refusal a part answers from `rehearsal` when it cannot take a snapshot. The text is
    /// fixed and names no data.
    #[must_use]
    pub fn rehearsal_unavailable(self) -> WorldError {
        WorldError(match self {
            PartName::Jobs => {
                concat!(
                    "world_access.rehearsal_unavailable",
                    ": rehearsal unavailable for jobs"
                )
            }
            PartName::Cache => {
                concat!(
                    "world_access.rehearsal_unavailable",
                    ": rehearsal unavailable for cache"
                )
            }
            PartName::Index => {
                concat!(
                    "world_access.rehearsal_unavailable",
                    ": rehearsal unavailable for index"
                )
            }
            PartName::Edge => {
                concat!(
                    "world_access.rehearsal_unavailable",
                    ": rehearsal unavailable for edge"
                )
            }
            PartName::Quarantine => concat!(
                "world_access.rehearsal_unavailable",
                ": rehearsal unavailable for quarantine"
            ),
            PartName::Cases => {
                concat!(
                    "world_access.rehearsal_unavailable",
                    ": rehearsal unavailable for cases"
                )
            }
            PartName::Documents => concat!(
                "world_access.rehearsal_unavailable",
                ": rehearsal unavailable for documents"
            ),
        })
    }
}

impl std::fmt::Display for PartName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The runtime handle a part over asynchronous storage blocks on, read while the site is built
/// (inside the runtime). `None` outside a runtime: the part then has no way to block, so its
/// `from_site` returns `None` rather than a part that cannot work.
#[must_use]
pub fn capture_runtime() -> Option<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().ok()
}

/// The jobs part: the real [`JobOps`] over the job store.
pub trait JobsPart: Send + Sync {
    /// The port over the real job store.
    fn live(&self) -> Box<dyn JobOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn JobOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the job store in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn JobOps>, WorldError>;
}

/// The cache part: the real [`CacheOps`] over the cache service.
pub trait CachePart: Send + Sync {
    /// The port over the real cache.
    fn live(&self) -> Box<dyn CacheOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn CacheOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the cache namespaces in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn CacheOps>, WorldError>;
}

/// The retrieval-index part: the real [`IndexOps`] over the retrieval service.
pub trait IndexPart: Send + Sync {
    /// The port over the real retrieval index.
    fn live(&self) -> Box<dyn IndexOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn IndexOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the index state in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn IndexOps>, WorldError>;
}

/// The edge-sync part: the real [`EdgeOps`] over the sync services.
pub trait EdgePart: Send + Sync {
    /// The port over the real edge sync.
    fn live(&self) -> Box<dyn EdgeOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn EdgeOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the edge positions in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn EdgeOps>, WorldError>;
}

/// The quarantine part: the real [`QuarantineOps`] over the quarantine store.
pub trait QuarantinePart: Send + Sync {
    /// The port over the real quarantine store.
    fn live(&self) -> Box<dyn QuarantineOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn QuarantineOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the quarantined items in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn QuarantineOps>, WorldError>;
}

/// The cases part: the real [`CaseOps`] over the BPM service.
pub trait CasesPart: Send + Sync {
    /// The port over the real BPM cases.
    fn live(&self) -> Box<dyn CaseOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn CaseOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the case states in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn CaseOps>, WorldError>;
}

/// The documents part: the real [`DocumentOps`] over the document lifecycle.
pub trait DocumentsPart: Send + Sync {
    /// The port over the real document lifecycle.
    fn live(&self) -> Box<dyn DocumentOps + '_>;

    /// The port over the real subsystem for a call made by `actor` (`agent:<id>`, the remedy's
    /// own identity). A part whose subsystem records or checks who acts (quarantine decisions,
    /// BPM cases, the document lifecycle) overrides this; the default is [`live`](Self::live).
    fn live_as(&self, actor: &str) -> Box<dyn DocumentOps + '_> {
        let _ = actor;
        self.live()
    }

    /// A snapshot of the document states in the in-memory fixture.
    ///
    /// # Errors
    ///
    /// [`PartName::rehearsal_unavailable`] when the part cannot snapshot.
    fn rehearsal(&self) -> Result<Box<dyn DocumentOps>, WorldError>;
}
