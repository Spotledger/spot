//! Typed errors for `spot-meta-live` (one enum, stable `metalive.*` codes).
//!
//! Every failure carries enough to locate it without parsing the message: the site and version
//! for a divergence, the full error lists for a failed build. Callers match on
//! [`LiveError::code`], never on the text — the codes are this crate's API contract and never
//! change in a breaking way (see `crates/framework/spot-errors/catalogue/metalive.toml`).

use spot_core::SiteId;
use spot_meta::MetaError;
use spot_plan::PlanError;

use crate::snapshot::{MetaHash, Version};

/// Result of every fallible operation in this crate.
pub type LiveResult<T> = Result<T, LiveError>;

/// One problem with live metadata.
///
/// `#[non_exhaustive]`: new variants may be added and callers must keep a catch-all arm.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LiveError {
    /// Two nodes hold the same metadata version number with different content. The receiving
    /// node refuses to guess and keeps serving its own snapshot.
    #[error(
        "site `{site}`: metadata version {version} conflicts: content {theirs} on the peer, {ours} here"
    )]
    Divergence {
        /// The site both nodes serve.
        site: String,
        /// The version they disagree about.
        version: Version,
        /// This node's content hash.
        ours: MetaHash,
        /// The content hash the peer announced.
        theirs: MetaHash,
    },
    /// Building the new snapshot failed. The old snapshot is still serving. Every error found
    /// is reported at once: the metadata errors, and — when the metadata built but the plans
    /// did not — the plan errors.
    #[error(
        "metadata version {version} does not build: {} metadata error(s), {} plan error(s)",
        meta.len(),
        plans.len()
    )]
    Build {
        /// The version the failed build would have become.
        version: Version,
        /// Every metadata error found, when the registry refused the definitions.
        meta: Vec<MetaError>,
        /// Every plan error found, when the metadata built but the plans did not compile.
        plans: Vec<PlanError>,
    },
    /// A change asked to remove a custom field the site does not have.
    #[error("`{doctype}.{field}` is not a custom field of this site, so it cannot be removed")]
    UnknownCustomField {
        /// The DocType the removal named.
        doctype: String,
        /// The field the removal named.
        field: String,
    },
    /// The version store failed to read or write.
    #[error("the metadata version store failed: {message}")]
    Store {
        /// What the store reported, in plain language.
        message: String,
    },
    /// The definitions could not be encoded to compute their content hash.
    #[error("the metadata could not be encoded for hashing: {message}")]
    Encode {
        /// What the serialiser reported.
        message: String,
    },
    /// The site's metadata version space (`u64`) is exhausted; a new version cannot be minted.
    #[error("the site's metadata version space is exhausted")]
    Exhausted,
}

impl LiveError {
    /// The stable machine-readable code for this error, for logs and API errors. Every code is
    /// `metalive.<snake_case>`; two variants never share one.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Divergence { .. } => "metalive.divergence",
            Self::Build { .. } => "metalive.build",
            Self::UnknownCustomField { .. } => "metalive.unknown_custom_field",
            Self::Store { .. } => "metalive.store",
            Self::Encode { .. } => "metalive.encode",
            Self::Exhausted => "metalive.exhausted",
        }
    }

    /// Whether this is the divergence the cross-node check exists to catch.
    #[must_use]
    pub fn is_divergence(&self) -> bool {
        matches!(self, Self::Divergence { .. })
    }
}

/// Builds a [`LiveError::Divergence`] for `site`.
pub(crate) fn divergence(
    site: &SiteId,
    version: Version,
    ours: &MetaHash,
    theirs: &MetaHash,
) -> LiveError {
    LiveError::Divergence {
        site: site.as_str().to_owned(),
        version,
        ours: ours.clone(),
        theirs: theirs.clone(),
    }
}
