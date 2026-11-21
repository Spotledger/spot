//! The harness' own failure type.
//!
//! The crate is a test harness: it defines no `spot_*` error codes (nothing here is reachable
//! from a running product), so it carries one plain error with a message that names the file,
//! object or schema that went wrong and never a document's contents.

use std::fmt;

/// A fixture or harness failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatError {
    /// What went wrong, naming the file, object or schema involved.
    message: String,
}

impl CompatError {
    /// A failure described by `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CompatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompatError {}

impl From<spot_journal_store::StoreError> for CompatError {
    fn from(error: spot_journal_store::StoreError) -> Self {
        Self::new(format!("the journal store refused: {error}"))
    }
}

impl From<spot_journal_manifest::ManifestError> for CompatError {
    fn from(error: spot_journal_manifest::ManifestError) -> Self {
        Self::new(format!("the manifest is not usable: {error}"))
    }
}

impl From<spot_journal_snapshot::SnapshotError> for CompatError {
    fn from(error: spot_journal_snapshot::SnapshotError) -> Self {
        Self::new(format!("the snapshot is not usable: {error}"))
    }
}

impl From<spot_journal_segment::SegmentError> for CompatError {
    fn from(error: spot_journal_segment::SegmentError) -> Self {
        Self::new(format!("the segment cannot be written: {error}"))
    }
}

impl From<spot_journal_rebuild_plan::PlanError> for CompatError {
    fn from(error: spot_journal_rebuild_plan::PlanError) -> Self {
        Self::new(format!("the journal cannot be planned: {error}"))
    }
}

impl From<spot_image_upcast::UpcastError> for CompatError {
    fn from(error: spot_image_upcast::UpcastError) -> Self {
        Self::new(format!("an image cannot be upcast: {error}"))
    }
}
