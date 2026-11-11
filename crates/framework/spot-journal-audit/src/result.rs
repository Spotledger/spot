//! The audit's result, and the condition a mismatch raises (ADR-067 s2).
//!
//! Everything here is plain data: counts, the first differing record, and the
//! `JOURNAL_AUDIT_MISMATCH` condition the conditions framework consumes later. No free text is
//! carried, only keys, counts and the record's identity.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The condition a failed journal audit raises (ADR-067 s2).
pub const JOURNAL_AUDIT_MISMATCH: &str = "JOURNAL_AUDIT_MISMATCH";

/// How one record differs between the live database and the scratch rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifferenceKind {
    /// The live database has the record and the rebuild does not.
    MissingInRebuild,
    /// The rebuild has the record and the live database does not.
    ExtraInRebuild,
    /// Both have it at different versions.
    VersionDiffers,
    /// Both have it at the same version with a different content digest.
    ContentDiffers,
}

impl DifferenceKind {
    /// The kind's stable name, the same one its JSON form uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingInRebuild => "missing_in_rebuild",
            Self::ExtraInRebuild => "extra_in_rebuild",
            Self::VersionDiffers => "version_differs",
            Self::ContentDiffers => "content_differs",
        }
    }
}

/// The first record that differs, in sort order (entity, then id): the record ADR-061 s8 makes
/// the audit name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstDifference {
    /// The entity of the record.
    pub entity: String,
    /// The id of the record.
    pub id: String,
    /// How it differs.
    pub kind: DifferenceKind,
    /// Its version in the live database, when the live database has it.
    pub live_version: Option<u64>,
    /// Its version in the rebuild, when the rebuild has it.
    pub rebuilt_version: Option<u64>,
}

impl FirstDifference {
    /// The canonical `entity/id` label the condition and a log line use.
    #[must_use]
    pub fn record(&self) -> String {
        format!("{}/{}", self.entity, self.id)
    }
}

/// The audit's outcome: the two databases agree, or the first record that shows they do not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AuditOutcome {
    /// Every compared record is present on both sides at the same version with the same digest.
    Match,
    /// At least one record differs; this is the first in sort order.
    Mismatch {
        /// The first differing record in sort order.
        first_difference: FirstDifference,
    },
}

impl AuditOutcome {
    /// Whether the two databases agree.
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }

    /// The first differing record, if there is one.
    #[must_use]
    pub fn first_difference(&self) -> Option<&FirstDifference> {
        match self {
            Self::Match => None,
            Self::Mismatch { first_difference } => Some(first_difference),
        }
    }

    /// The `JOURNAL_AUDIT_MISMATCH` condition (ADR-067 s2) for a mismatch, as plain data for the
    /// conditions framework; `None` when the audit matched, because a match raises nothing.
    ///
    /// `subject` names what the condition is about (ADR-067 s1): the tenant, or the tenant's
    /// stream. The audit itself has no tenant, so the caller gives it.
    #[must_use]
    pub fn to_condition(&self, subject: &str) -> Option<Condition> {
        let first_differing_record = self.first_difference()?.clone();
        Some(Condition {
            id: JOURNAL_AUDIT_MISMATCH.to_owned(),
            subject: subject.to_owned(),
            params: ConditionParams {
                first_differing_record,
            },
        })
    }
}

/// A condition as the conditions framework consumes it (ADR-067 s1): a stable id, its subject, and
/// the parameters its explanation template braces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    /// The stable condition id, `JOURNAL_AUDIT_MISMATCH` here.
    pub id: String,
    /// What the condition is about: a node, a stream, an entity, a record or the tenant.
    pub subject: String,
    /// The parameters the condition's explanation template uses.
    pub params: ConditionParams,
}

/// The parameters of a journal audit condition: the record that differs first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionParams {
    /// The first differing record in sort order.
    pub first_differing_record: FirstDifference,
}

/// The counts of one entity, or of the whole audit, as [`compare`](crate::compare) reports them.
///
/// `live` and `rebuilt` count the records each side holds; the four difference counts add up to
/// the number of records that do not agree, and a record counted as missing or extra is counted
/// once, not as a version and a content difference too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityCount {
    /// Records the live database holds, derived entities excluded.
    pub live: u64,
    /// Records the rebuild holds, derived entities excluded.
    pub rebuilt: u64,
    /// Records only the live database holds.
    pub missing_in_rebuild: u64,
    /// Records only the rebuild holds.
    pub extra_in_rebuild: u64,
    /// Records both hold at different versions.
    pub version_differs: u64,
    /// Records both hold at the same version with a different content digest.
    pub content_differs: u64,
}

impl EntityCount {
    /// Every record that does not agree between the two sides.
    #[must_use]
    pub fn differences(&self) -> u64 {
        self.missing_in_rebuild
            + self.extra_in_rebuild
            + self.version_differs
            + self.content_differs
    }

    /// Whether this entity (or the whole audit) agrees.
    #[must_use]
    pub fn is_match(&self) -> bool {
        self.differences() == 0 && self.live == self.rebuilt
    }

    /// Adds another count into this one.
    pub fn add(&mut self, other: &EntityCount) {
        self.live += other.live;
        self.rebuilt += other.rebuilt;
        self.missing_in_rebuild += other.missing_in_rebuild;
        self.extra_in_rebuild += other.extra_in_rebuild;
        self.version_differs += other.version_differs;
        self.content_differs += other.content_differs;
    }
}

/// The whole comparison: the outcome, the per-entity counts, their total, and what was skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditResult {
    /// Whether the two databases agree, or the first record that shows they do not.
    pub outcome: AuditOutcome,
    /// Counts per entity, in entity order; an entity appears when it holds a record on either
    /// side.
    pub per_entity: BTreeMap<String, EntityCount>,
    /// The per-entity counts summed.
    pub totals: EntityCount,
    /// The derived entities the audit skipped and the catalog flagged, in name order; an entity
    /// flagged but holding no record is not listed, because nothing was skipped.
    pub derived_skipped: Vec<String>,
}

impl AuditResult {
    /// Whether the two databases agree.
    #[must_use]
    pub fn is_match(&self) -> bool {
        self.outcome.is_match()
    }

    /// The counts of one entity, if it holds a record on either side.
    #[must_use]
    pub fn entity(&self, entity: &str) -> Option<&EntityCount> {
        self.per_entity.get(entity)
    }
}
