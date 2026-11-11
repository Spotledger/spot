//! Stable `cloud_backup.*` error codes, catalogued in
//! `crates/framework/spot-errors/catalogue/cloud_backup.toml`.
//!
//! Codes never change; callers match [`CloudBackupError::code`], not the message. A
//! message names a backup id, a chunk index or a stage, never a key, a recovery phrase
//! or a byte of plaintext: the recovery key and the data key exist only as redacted
//! values.

use std::fmt;

/// A cloud backup, upload, verification, retention or restore failure with a stable code.
#[derive(Debug, thiserror::Error)]
pub enum CloudBackupError {
    /// The tenant's licence does not grant cloud backup. Code
    /// `cloud_backup.not_entitled`.
    #[error("the licence for tenant {tenant} does not grant cloud backup")]
    NotEntitled {
        /// The tenant whose licence was checked.
        tenant: String,
    },
    /// The upload would pass the tenant's storage quota. Code
    /// `cloud_backup.quota_exceeded`.
    #[error(
        "the backup needs {needed} bytes but only {remaining} of the tenant quota remain (limit {limit})"
    )]
    QuotaExceeded {
        /// Bytes the upload needs.
        needed: u64,
        /// Bytes the tenant may still use.
        remaining: u64,
        /// The tenant's quota in bytes.
        limit: u64,
    },
    /// An immutable object already exists under the key with different bytes. Code
    /// `cloud_backup.object_conflict`.
    #[error("object {key} already exists with different contents; cloud objects are immutable")]
    ObjectConflict {
        /// The object key.
        key: String,
    },
    /// A manifest names a chunk the store does not hold. Code `cloud_backup.chunk_missing`.
    #[error("chunk {index} of backup {backup_id} is missing from the store")]
    ChunkMissing {
        /// The backup whose chunk is missing.
        backup_id: String,
        /// The chunk index.
        index: u32,
    },
    /// A chunk failed its digest or its authentication tag. Code
    /// `cloud_backup.chunk_corrupt`.
    #[error("chunk {index} of backup {backup_id} is corrupted: {reason}")]
    ChunkCorrupt {
        /// The backup whose chunk is corrupt.
        backup_id: String,
        /// The chunk index.
        index: u32,
        /// What failed (digest or authentication).
        reason: &'static str,
    },
    /// A manifest is missing fields, has impossible values or is not canonical. Code
    /// `cloud_backup.manifest_invalid`.
    #[error("the manifest of backup {backup_id} is not valid: {reason}")]
    ManifestInvalid {
        /// The backup the manifest claims.
        backup_id: String,
        /// What is wrong.
        reason: String,
    },
    /// A manifest's authenticator did not verify under the backup's data key. Code
    /// `cloud_backup.manifest_signature`.
    #[error("the manifest of backup {backup_id} failed its signature check")]
    ManifestSignature {
        /// The backup whose manifest failed.
        backup_id: String,
    },
    /// The supplied recovery key does not open this backup. Code
    /// `cloud_backup.wrong_key`.
    #[error("the recovery key does not open backup {backup_id}")]
    WrongKey {
        /// The backup that could not be opened.
        backup_id: String,
    },
    /// No manifest exists for the requested backup. Code `cloud_backup.not_found`.
    #[error("no backup {backup_id} exists for tenant {tenant}")]
    NotFound {
        /// The requested backup id.
        backup_id: String,
        /// The tenant that asked.
        tenant: String,
    },
    /// The underlying object store failed. Code `cloud_backup.store`.
    #[error("the backup store failed: {reason}")]
    Store {
        /// Plain-language reason, never a key or plaintext.
        reason: String,
    },
    /// A recovery phrase is malformed or its checksum does not match. Code
    /// `cloud_backup.recovery_phrase`.
    #[error("the recovery phrase is not valid: {reason}")]
    RecoveryPhrase {
        /// What is wrong.
        reason: &'static str,
    },
    /// A delete did not carry a permit from a retention plan. Code
    /// `cloud_backup.delete_refused`.
    #[error("deleting backup {backup_id} was refused: {reason}")]
    DeleteRefused {
        /// The backup someone tried to delete.
        backup_id: String,
        /// Why the delete was refused.
        reason: String,
    },
    /// A retention policy or plan is invalid. Code `cloud_backup.retention`.
    #[error("retention refused: {reason}")]
    Retention {
        /// What makes the policy or plan unusable.
        reason: String,
    },
    /// An operation arrived in the wrong state (for example a commit before every chunk
    /// was uploaded). Code `cloud_backup.state`.
    #[error("backup {backup_id} is in the wrong state for {operation}: {reason}")]
    InvalidState {
        /// The backup concerned.
        backup_id: String,
        /// The operation that was attempted.
        operation: &'static str,
        /// Why the state does not allow it.
        reason: String,
    },
    /// Post-upload verification failed after every retry. Code
    /// `cloud_backup.verify_failed`.
    #[error("backup {backup_id} failed verification: {reason}")]
    VerifyFailed {
        /// The backup that failed.
        backup_id: String,
        /// What failed.
        reason: String,
    },
    /// A restore step failed. Code `cloud_backup.restore`.
    #[error("restoring backup {backup_id} failed at {stage}: {reason}")]
    Restore {
        /// The backup being restored.
        backup_id: String,
        /// The stage that failed.
        stage: &'static str,
        /// Plain-language reason.
        reason: String,
    },
}

impl CloudBackupError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotEntitled { .. } => "cloud_backup.not_entitled",
            Self::QuotaExceeded { .. } => "cloud_backup.quota_exceeded",
            Self::ObjectConflict { .. } => "cloud_backup.object_conflict",
            Self::ChunkMissing { .. } => "cloud_backup.chunk_missing",
            Self::ChunkCorrupt { .. } => "cloud_backup.chunk_corrupt",
            Self::ManifestInvalid { .. } => "cloud_backup.manifest_invalid",
            Self::ManifestSignature { .. } => "cloud_backup.manifest_signature",
            Self::WrongKey { .. } => "cloud_backup.wrong_key",
            Self::NotFound { .. } => "cloud_backup.not_found",
            Self::Store { .. } => "cloud_backup.store",
            Self::RecoveryPhrase { .. } => "cloud_backup.recovery_phrase",
            Self::DeleteRefused { .. } => "cloud_backup.delete_refused",
            Self::Retention { .. } => "cloud_backup.retention",
            Self::InvalidState { .. } => "cloud_backup.state",
            Self::VerifyFailed { .. } => "cloud_backup.verify_failed",
            Self::Restore { .. } => "cloud_backup.restore",
        }
    }

    /// A store failure from any underlying error, message kept free of key material.
    pub(crate) fn store(error: impl fmt::Display) -> Self {
        Self::Store {
            reason: error.to_string(),
        }
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, CloudBackupError>;
