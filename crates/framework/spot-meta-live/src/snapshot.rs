//! The metadata version, the content hash, and one immutable [`Snapshot`].
//!
//! A [`Snapshot`] is the unit the engine reads through: an `Arc<EffectiveMeta>`, its compiled
//! `Arc<PlanSet>`, the site version and a content hash of the definitions. It is immutable and
//! built whole, so a reader that holds the `Arc` sees one consistent set however many swaps
//! happen meanwhile (ADR-05).

use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use spot_meta::{
    AMENDED_FROM, BuildOptions, Customisations, DOC_STATE, Definition, EffectiveMeta, Registry,
};
use spot_plan::{DocPlan, PlanSet};

use crate::error::{LiveError, LiveResult};

/// The definitions of one snapshot, as `spot-meta` owns them.
pub type Definitions = Vec<Definition>;

/// A site's metadata version.
///
/// Versions are minted per site and rise strictly: every successful [`crate::LiveMeta::apply`]
/// and every adopted snapshot moves forward by at least one. The version is the site's, not
/// `spot-meta`'s local build counter — a follower sets it to the version the version store
/// announced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(u64);

impl Version {
    /// The version of a node that has not applied or adopted anything yet.
    pub const ZERO: Version = Version(0);

    /// Wraps a raw version number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw version number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next version, or `None` when the `u64` space is exhausted. Callers must never wrap:
    /// a version that went backwards would let a stale invalidation look fresh.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for Version {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Version> for u64 {
    fn from(version: Version) -> Self {
        version.0
    }
}

/// SHA-256 of a snapshot's definitions, as 64 lowercase hex characters.
///
/// Two builds of the same metadata content have the same hash; any real change (a field, a
/// rule, a constraint, a custom override) changes it. The version is **not** part of the hash:
/// that is exactly what lets two nodes tell "same version number, different content" apart from
/// "same content" (see [`LiveError::Divergence`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MetaHash(String);

impl MetaHash {
    /// The content hash of `definitions`, over their canonical (JSON) form. Definitions are
    /// already sorted by name and carry their fields in declaration order, so the form is
    /// deterministic for equal content.
    ///
    /// # Errors
    ///
    /// [`LiveError::Encode`] when the definitions cannot be serialised, which cannot happen for
    /// a value built by `spot-meta` and is reported rather than panicked.
    pub fn of_definitions(definitions: &[Definition]) -> LiveResult<Self> {
        let bytes = serde_json::to_vec(definitions).map_err(|error| LiveError::Encode {
            message: error.to_string(),
        })?;
        Ok(Self::of_bytes(&bytes))
    }

    /// Wraps a hex digest read back from a version store. The digest is compared, never parsed,
    /// so no validation happens here.
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The 64 lowercase hex characters.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }

    fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hex(&hasher.finalize()))
    }
}

impl fmt::Display for MetaHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowercase hex of a digest or of raw bytes, written without floats or fancy code.
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(*byte >> 4)] as char);
        out.push(HEX[usize::from(*byte & 0x0f)] as char);
    }
    out
}

/// The effective metadata at one version: the merged definitions, their compiled plans, the
/// site version and the content hash.
///
/// Immutable. A change builds a whole new snapshot and swaps it in; a request that took an
/// `Arc<Snapshot>` keeps seeing exactly this one (ADR-05). Nothing here is half-built: the
/// plans compile before the snapshot exists.
#[derive(Clone, Debug)]
pub struct Snapshot {
    version: Version,
    hash: MetaHash,
    meta: Arc<EffectiveMeta>,
    plans: Arc<PlanSet>,
}

impl Snapshot {
    /// Builds the metadata and its plans, then hashes the definitions. The snapshot is
    /// returned only when **everything** built: one failing DocType produces no snapshot (and
    /// reports every problem it found).
    ///
    /// The metadata is stamped with `version` (`Customisations::stamp_revision`), so
    /// `meta().version() == version.get()` for every version of 1 or more, whichever path built
    /// the snapshot; the caller's customisations are not modified.
    ///
    /// # Errors
    ///
    /// [`LiveError::Build`] with the metadata errors, or the plan errors when the metadata
    /// built but a plan did not.
    pub fn build(
        definitions: Definitions,
        customisations: &Customisations,
        options: &BuildOptions,
        version: Version,
    ) -> LiveResult<Self> {
        // The live layer owns the numbering: the metadata is stamped with the snapshot's own
        // version (`revision = version - 1`), so every path that builds a snapshot (`apply`,
        // `adopt`, `spot-customise`) reports `meta().version() == version` at version 1 or
        // above. Version 0 keeps the revision it was given (the registry cannot report 0).
        let stamped;
        let customisations = if version.get() >= 1 {
            let mut copy = customisations.clone();
            copy.stamp_revision(version.get() - 1);
            stamped = copy;
            &stamped
        } else {
            customisations
        };
        let meta = match Registry::build_with(definitions, customisations, options) {
            Ok(meta) => meta,
            Err(meta) => {
                return Err(LiveError::Build {
                    version,
                    meta,
                    plans: Vec::new(),
                });
            }
        };
        let plans = match PlanSet::build(&meta) {
            Ok(plans) => plans,
            Err(plans) => {
                return Err(LiveError::Build {
                    version,
                    meta: Vec::new(),
                    plans,
                });
            }
        };
        let hash = MetaHash::of_definitions(meta.definitions())?;
        Ok(Self {
            version,
            hash,
            meta: Arc::new(meta),
            plans: Arc::new(plans),
        })
    }

    /// The site version this snapshot serves.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// The content hash of its definitions.
    #[must_use]
    pub fn hash(&self) -> &MetaHash {
        &self.hash
    }

    /// The effective metadata.
    #[must_use]
    pub fn meta(&self) -> &Arc<EffectiveMeta> {
        &self.meta
    }

    /// The compiled plans.
    #[must_use]
    pub fn plans(&self) -> &Arc<PlanSet> {
        &self.plans
    }

    /// One DocType's compiled plan.
    #[must_use]
    pub fn plan(&self, doctype: &str) -> Option<Arc<DocPlan>> {
        self.plans.get(doctype)
    }
}

/// Drops the engine-managed fields from a posted DocType's definitions, so a reloaded snapshot
/// can be rebuilt.
///
/// `spot-meta` **adds** `doc_state` and `amended_from` to every posted DocType at build time.
/// A follower loads the *effective* definitions of the announced version — which already contain
/// those fields — and [`LiveMeta::adopt`](crate::LiveMeta::adopt) calls this before rebuilding
/// them; without it, the registry would add a second copy and the build would fail. Only the
/// adopt path strips: the apply path builds authored definitions, where an engine field must stay
/// a validation error. The registry always puts exactly one copy back, so the result is the same
/// effective metadata the publisher built (and the same content hash).
pub(crate) fn without_engine_fields(mut definitions: Definitions) -> Definitions {
    for definition in &mut definitions {
        if definition.is_posted {
            definition
                .fields
                .retain(|field| field.name != DOC_STATE && field.name != AMENDED_FROM);
        }
    }
    definitions
}
