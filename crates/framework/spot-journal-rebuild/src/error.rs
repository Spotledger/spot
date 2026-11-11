//! Typed errors with stable `journal_rebuild.*` codes.
//!
//! A message names a tenant-neutral thing: a generation, a position, an entity, a record id, a
//! schema version or another crate's stable code. It never carries an image, a field value or
//! object bytes.

/// Why a rebuild did not complete.
///
/// Until [`crate::ApplySink::begin`] has been called nothing was applied; every refusal that can
/// be found from the plan, the manifests, the snapshot index and the segments (including a
/// missing upcaster, a gap, a foreign entry and a malformed entry) is found before that point.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RebuildError {
    /// The options are unusable.
    #[error("the rebuild options are not usable: {0}")]
    OptionsInvalid(&'static str),
    /// The plan contradicts itself.
    #[error("the rebuild plan is not usable: {0}")]
    PlanInvalid(&'static str),
    /// The journal verification found evidence of tampering or loss.
    #[error(
        "the journal does not verify: {count} finding(s), the first is {kind} for generation {generation} positions {first_seq}..={last_seq}"
    )]
    VerifyFailed {
        /// The code of the first finding (`journal_verify.<name>`).
        kind: &'static str,
        /// The generation the first finding names.
        generation: u64,
        /// The first position of its range.
        first_seq: u64,
        /// The last position of its range.
        last_seq: u64,
        /// How many tampering findings the pass reported.
        count: usize,
    },
    /// The verification could not be carried out.
    #[error("the journal verification could not run: {code}")]
    VerifyUnavailable {
        /// The code of the verification crate's error.
        code: &'static str,
    },
    /// A manifest is missing, malformed, untrusted, or belongs to another source or generation.
    #[error("the manifest of generation {generation} cannot be trusted: {reason}")]
    ManifestInvalid {
        /// The generation asked for.
        generation: u64,
        /// What is wrong (a `journal_manifest.*` code, or a short phrase).
        reason: &'static str,
    },
    /// The snapshot cannot be read.
    #[error("the base snapshot cannot be read: {code}")]
    SnapshotFailed {
        /// The code of the snapshot crate's error.
        code: &'static str,
    },
    /// The object store failed.
    #[error("the journal store failed: {code}")]
    StoreFailed {
        /// The code of the store's error.
        code: &'static str,
    },
    /// A segment the plan needs is not in the store, or the manifest does not list it.
    #[error("no segment holds generation {generation} positions {first_seq}..={last_seq}")]
    SegmentMissing {
        /// The generation.
        generation: u64,
        /// The first position nothing holds.
        first_seq: u64,
        /// The last position of the range being read.
        last_seq: u64,
    },
    /// A segment is damaged, or does not match the manifest or the tenant.
    #[error(
        "the segment of generation {generation} positions {first_seq}..={last_seq} is invalid: {reason}"
    )]
    SegmentInvalid {
        /// The generation.
        generation: u64,
        /// The first position of the segment.
        first_seq: u64,
        /// The last position of the segment.
        last_seq: u64,
        /// What is wrong.
        reason: &'static str,
    },
    /// An entry names another source than the plan's.
    #[error("the entry at generation {generation} position {seq} belongs to another source")]
    ForeignEntry {
        /// The generation of the entry.
        generation: u64,
        /// The position of the entry.
        seq: u64,
    },
    /// An entry is not a well-formed envelope or state payload.
    #[error("the entry at generation {generation} position {seq} is invalid: {reason}")]
    EntryInvalid {
        /// The generation of the entry.
        generation: u64,
        /// The position of the entry.
        seq: u64,
        /// What is wrong.
        reason: &'static str,
    },
    /// Positions are not strictly consecutive.
    #[error("generation {generation}: expected position {expected} but found {found}")]
    PositionGap {
        /// The generation of the entry (or step) that broke the sequence.
        generation: u64,
        /// The position the executor expected next.
        expected: u64,
        /// The position it found.
        found: u64,
    },
    /// A record's version went backwards.
    #[error(
        "{entity} {id}: version {found} at generation {generation} position {seq} is not above the applied version {applied}"
    )]
    VersionRegressed {
        /// The entity.
        entity: String,
        /// The record id.
        id: String,
        /// The generation of the entry.
        generation: u64,
        /// The position of the entry.
        seq: u64,
        /// The version already applied.
        applied: u64,
        /// The version found.
        found: u64,
    },
    /// An image cannot be brought to the target schema: no upcaster step.
    #[error("{entity}: no upcaster from schema {from} towards schema {to}")]
    MissingUpcast {
        /// The entity.
        entity: String,
        /// The version the chain stops at.
        from: u64,
        /// The schema asked for.
        to: u64,
    },
    /// An upcaster refused an image.
    #[error("{entity}: the image cannot be upcast: {code}")]
    UpcastFailed {
        /// The entity.
        entity: String,
        /// The code of the upcast crate's error.
        code: &'static str,
    },
    /// The apply sink refused.
    #[error("the apply sink refused: {reason}")]
    SinkFailed {
        /// The sink's reason (the sink never puts a value in it).
        reason: String,
    },
    /// The caller cancelled the rebuild; the sink was told to abort.
    #[error("the rebuild was cancelled after {processed} entries")]
    Cancelled {
        /// How many entries and images had been handed over.
        processed: u64,
    },
}

impl RebuildError {
    /// The stable catalogue code of this refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OptionsInvalid(_) => "journal_rebuild.options_invalid",
            Self::PlanInvalid(_) => "journal_rebuild.plan_invalid",
            Self::VerifyFailed { .. } => "journal_rebuild.verify_failed",
            Self::VerifyUnavailable { .. } => "journal_rebuild.verify_unavailable",
            Self::ManifestInvalid { .. } => "journal_rebuild.manifest_invalid",
            Self::SnapshotFailed { .. } => "journal_rebuild.snapshot_failed",
            Self::StoreFailed { .. } => "journal_rebuild.store_failed",
            Self::SegmentMissing { .. } => "journal_rebuild.segment_missing",
            Self::SegmentInvalid { .. } => "journal_rebuild.segment_invalid",
            Self::ForeignEntry { .. } => "journal_rebuild.foreign_entry",
            Self::EntryInvalid { .. } => "journal_rebuild.entry_invalid",
            Self::PositionGap { .. } => "journal_rebuild.position_gap",
            Self::VersionRegressed { .. } => "journal_rebuild.version_regressed",
            Self::MissingUpcast { .. } => "journal_rebuild.missing_upcast",
            Self::UpcastFailed { .. } => "journal_rebuild.upcast_failed",
            Self::SinkFailed { .. } => "journal_rebuild.sink_failed",
            Self::Cancelled { .. } => "journal_rebuild.cancelled",
        }
    }
}
