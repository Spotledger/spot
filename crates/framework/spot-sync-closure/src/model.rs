//! Plain data: record keys, images, limits and the bundle plan.

use crate::error::ClosureError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A position in the source's event stream (the acknowledged position and each record's latest event).
pub type Position = u64;

/// The identity of one record: its entity and id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecordKey {
    /// The entity (DocType) name.
    pub entity: String,
    /// The record id.
    pub id: String,
}

impl RecordKey {
    /// Build a key.
    #[must_use]
    pub fn new(entity: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            entity: entity.into(),
            id: id.into(),
        }
    }
}

/// The current image of a record with the position of its latest event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceImage {
    /// Which record.
    pub key: RecordKey,
    /// The whole record, parent and child rows (full-state events, ADR-061).
    pub image: Value,
    /// The position of the record's latest event.
    pub position: Position,
}

impl SourceImage {
    /// Build an image.
    #[must_use]
    pub fn new(key: RecordKey, image: Value, position: Position) -> Self {
        Self {
            key,
            image,
            position,
        }
    }
}

/// Why a record is in a bundle (ADR-064 section 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// A document the user saved or submitted.
    Root,
    /// Another in-scope event of the same source transaction.
    Satellite,
    /// A dependency the destination lacks.
    Extra,
}

/// Whether a bundle carries the roots or only what they need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleKind {
    /// Extras only, sent before the root bundle when the closure is over the cap.
    Prerequisite,
    /// The roots and satellites (with the extras too, when everything fits one bundle).
    Root,
}

/// One record in a bundle, its image already redacted for the destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRecord {
    /// Why the record is here.
    pub role: Role,
    /// Which record.
    pub key: RecordKey,
    /// Position of the record's latest event.
    pub position: Position,
    /// The image after `excluded_fields.down`.
    pub image: Value,
}

/// One bundle: applied by the destination in one transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// Prerequisite or root.
    pub kind: BundleKind,
    /// Extras first in dependency order (targets before referrers), then satellites and roots.
    pub records: Vec<BundleRecord>,
    /// Total size of the redacted images (their compact JSON length).
    pub bytes: usize,
}

/// The bundles to send, in order: prerequisites first, the root bundle last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundlePlan {
    /// Bundles in the order the destination must apply them.
    pub bundles: Vec<Bundle>,
    /// The extras that are outside the destination's scope: record them in its extras set once
    /// delivered (`ExtrasSet::record_delivered_extra`) so later versions keep being routed.
    pub out_of_scope_extras: Vec<RecordKey>,
    /// Records withheld because a record their closure needs is held at the destination, as
    /// `(dependent, held record)`: one pair per held record that blocks a dependent, sorted by
    /// dependent and then by held record. Nothing of a withheld record is in `bundles`, because
    /// the destination could not apply it without the dependency (`sync_closure.held_dependency`).
    /// A caller that has holds stores the dependent like a held entry and releases it with the
    /// dependency.
    #[serde(default)]
    pub withheld_by_hold: Vec<(RecordKey, RecordKey)>,
}

/// Closure bounds (ADR-064 section 4). Settings, never hidden constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Most records in one bundle (default 500).
    pub max_records: usize,
    /// Most image bytes in one bundle (default 4 MiB).
    pub max_bytes: usize,
    /// More records than this in a closure is `CLOSURE_TOO_LARGE` (default 5,000).
    pub hard_limit_records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_records: 500,
            max_bytes: 4 * 1024 * 1024,
            hard_limit_records: 5_000,
        }
    }
}

impl Limits {
    /// Reject limits that cannot work.
    ///
    /// # Errors
    /// `InvalidLimits` when a limit is zero or the hard limit is below `max_records`.
    pub fn validate(&self) -> Result<(), ClosureError> {
        if self.max_records == 0 || self.max_bytes == 0 || self.hard_limit_records == 0 {
            return Err(ClosureError::InvalidLimits {
                reason: "every limit must be above zero",
            });
        }
        if self.hard_limit_records < self.max_records {
            return Err(ClosureError::InvalidLimits {
                reason: "the hard limit must not be below the per-bundle record cap",
            });
        }
        Ok(())
    }
}
