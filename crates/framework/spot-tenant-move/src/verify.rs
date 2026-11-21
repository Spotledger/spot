//! The invariant fingerprint comparison before cut-over.
//!
//! The fingerprint is `spot-rehearsal`'s: document counts and integer totals per
//! section, sequence high-water marks and blob digests all reach it as declared
//! [`spot_rehearsal::FingerprintSpec`] sections. A move measures the same spec on the
//! quiesced source and on the imported target; both the canonical digest and every row
//! must be identical. The comparison happens **before** the placement flip, and a
//! mismatch leaves the move in `Imported` with its source still serving.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_rehearsal::{Fingerprint, FingerprintSource as EngineSource, FingerprintSpec};

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::plan::MovePlan;

/// Which side of the move a fingerprint is measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The quiesced source.
    Source,
    /// The imported target.
    Target,
}

/// The outcome of one comparison, persisted in the move record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    /// The source fingerprint's canonical digest.
    pub source_digest: String,
    /// The target fingerprint's canonical digest.
    pub target_digest: String,
    /// How many fingerprint sections were measured.
    pub sections: u64,
    /// How many rows or measures differ.
    pub differences: u64,
    /// Whether the digests and every row are identical.
    pub matched: bool,
}

impl Verification {
    /// Compares two measured fingerprints.
    #[must_use]
    pub fn between(source: &Fingerprint, target: &Fingerprint) -> Self {
        let differences = source.diff(target);
        let source_digest = source.digest();
        let target_digest = target.digest();
        Self {
            matched: source_digest == target_digest && differences.is_empty(),
            source_digest,
            target_digest,
            sections: u64::try_from(source.sections.len()).unwrap_or(u64::MAX),
            differences: u64::try_from(differences.len()).unwrap_or(u64::MAX),
        }
    }
}

/// Measures the move's invariant fingerprint on one side.
pub trait FingerprintSource: Send + Sync {
    /// Measures the fingerprint of `side` for `plan`.
    ///
    /// # Errors
    /// [`Error::FingerprintFailed`] with a fixed reason.
    fn measure<'a>(&'a self, plan: &'a MovePlan, side: Side) -> BoxFuture<'a, Result<Fingerprint>>;
}

/// The adapter over `spot-rehearsal`'s fingerprint engine.
///
/// `spec` declares the move's sections (counts, totals, sequence high-water marks, blob
/// digests); `engine` runs the declared queries; the two `CloneTarget`s address the
/// source and target databases through the same source implementation.
pub struct RehearsalFingerprintSource {
    spec: FingerprintSpec,
    engine: Arc<dyn EngineSource>,
    targets: BTreeMap<Side, spot_rehearsal::CloneTarget>,
}

impl RehearsalFingerprintSource {
    /// An adapter over a spec, an engine source and the two database addresses.
    #[must_use]
    pub fn new(
        spec: FingerprintSpec,
        engine: Arc<dyn EngineSource>,
        source: spot_rehearsal::CloneTarget,
        target: spot_rehearsal::CloneTarget,
    ) -> Self {
        let mut targets = BTreeMap::new();
        targets.insert(Side::Source, source);
        targets.insert(Side::Target, target);
        Self {
            spec,
            engine,
            targets,
        }
    }
}

impl FingerprintSource for RehearsalFingerprintSource {
    fn measure<'a>(&'a self, plan: &'a MovePlan, side: Side) -> BoxFuture<'a, Result<Fingerprint>> {
        Box::pin(async move {
            let target = self.targets.get(&side).ok_or(Error::FingerprintFailed {
                tenant: plan.tenant.to_string(),
                reason: "the fingerprint address for that side is missing",
            })?;
            let fingerprint = self
                .spec
                .measure(&*self.engine, target)
                .await
                .map_err(|_| Error::FingerprintFailed {
                    tenant: plan.tenant.to_string(),
                    reason: "the fingerprint engine failed",
                })?;
            // An empty spec measures nothing on both sides and would "match" vacuously.
            if fingerprint.sections.is_empty() {
                return Err(Error::FingerprintFailed {
                    tenant: plan.tenant.to_string(),
                    reason: "the fingerprint spec declares no section",
                });
            }
            Ok(fingerprint)
        })
    }
}
