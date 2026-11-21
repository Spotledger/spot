//! Typed errors with stable `journal_snapshot_pg.*` codes. A message never carries a record's
//! content.

use spot_journal_snapshot::SnapshotError;

/// Everything that stops a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotPgError {
    /// The database could not be read (a statement failed, the transaction could not start).
    #[error("the snapshot read failed: {0}")]
    ReadFailed(&'static str),
    /// A stored value contradicts its declared type, so the image cannot be written.
    #[error("a stored value cannot be written as an image: {0}")]
    ImageInvalid(&'static str),
    /// The catalogue or the options are unusable.
    #[error("the snapshot options are invalid: {0}")]
    OptionsInvalid(&'static str),
    /// The journal did not reach the snapshot's position in time: the signed index is not
    /// written, so nothing is listed.
    #[error("the journal did not reach the snapshot's position in time")]
    JournalBehind,
    /// The snapshot writer or the store failed (code of the snapshot crate).
    #[error("the snapshot could not be written: {0}")]
    Write(&'static str),
    /// The signing or verifying key could not be derived.
    #[error("the snapshot key could not be derived")]
    Key,
}

impl SnapshotPgError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ReadFailed(_) => "journal_snapshot_pg.read_failed",
            Self::ImageInvalid(_) => "journal_snapshot_pg.image_invalid",
            Self::OptionsInvalid(_) => "journal_snapshot_pg.options_invalid",
            Self::JournalBehind => "journal_snapshot_pg.journal_behind",
            Self::Write(_) => "journal_snapshot_pg.write_failed",
            Self::Key => "journal_snapshot_pg.key_unavailable",
        }
    }
}

impl From<SnapshotError> for SnapshotPgError {
    fn from(error: SnapshotError) -> Self {
        Self::Write(error.code())
    }
}
