//! Stable `backup.*` error codes (ADR-055), catalogued in
//! `crates/framework/spot-errors/catalogue/backup.toml`.
//!
//! Codes never change; callers match [`BackupError::code`], not the message. Messages
//! name paths, targets and reasons but never a secret: an encryption reference is an
//! identifier, and its key material never enters this crate.

use std::fmt;

/// A backup, verification, retention or restore failure with a stable code.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// A removable target's media is absent or was unplugged. Code
    /// `backup.target_unavailable`.
    #[error("the backup target {target} is not available: {reason}")]
    TargetUnavailable {
        /// The target that is not present (its volume id or name).
        target: String,
        /// Plain-language reason (for example "the volume is not inserted").
        reason: String,
    },
    /// A removable target sits on the same physical disk as the live data and the
    /// setting forbids that. Code `backup.same_physical_disk`.
    #[error("the backup target {target} is on the live data's physical disk {disk}")]
    SamePhysicalDisk {
        /// The target that was refused.
        target: String,
        /// The shared physical disk identifier.
        disk: String,
    },
    /// A manifest or package is missing required, well-formed fields. Code
    /// `backup.manifest_invalid`.
    #[error("the backup manifest is not valid: {reason}")]
    ManifestInvalid {
        /// What is wrong.
        reason: String,
    },
    /// A manifest digest, size or header check failed. Code `backup.verify_failed`.
    #[error("backup verification failed: {reason}")]
    VerifyFailed {
        /// What failed and where.
        reason: String,
    },
    /// Retention planning refused to delete a protected backup. Code
    /// `backup.retention_refused`.
    #[error("retention refused to delete {id}: {reason}")]
    RetentionRefused {
        /// The backup that was protected.
        id: String,
        /// Why it is protected.
        reason: String,
    },
    /// The restore preflight found one or more blockers. Code
    /// `backup.restore_refused`.
    #[error("the restore was refused before any data changed: {reason}")]
    RestoreRefused {
        /// The joined preflight blockers.
        reason: String,
    },
    /// The backup was taken by a newer platform schema than the target runs. Code
    /// `backup.newer_schema`.
    #[error(
        "the backup schema {backup_schema} is newer than the target schema {target_schema}; pass allow_newer_schema to override"
    )]
    NewerSchema {
        /// Schema version recorded by the backup.
        backup_schema: u64,
        /// Schema version the target runtime runs.
        target_schema: u64,
    },
    /// A backup id is unknown to its target. Code `backup.not_found`.
    #[error("no backup {id} exists on the target")]
    NotFound {
        /// The requested backup id.
        id: String,
    },
    /// The restored data failed its invariant checks. Code
    /// `backup.invariants_failed`.
    #[error("the restore finished but invariants failed: {violations}")]
    InvariantsFailed {
        /// One line per violation, joined.
        violations: String,
    },
    /// A restore stopped at a named stage; the service gate stays closed. Code
    /// `backup.restore_failed`.
    #[error("the restore failed at {stage}: {reason}")]
    RestoreFailed {
        /// The stage that failed (`apply`, `migrate`, `schema_check`, `open`).
        stage: String,
        /// Plain-language reason.
        reason: String,
    },
    /// A feature this build does not provide was requested. Code
    /// `backup.unsupported`.
    #[error("the requested backup feature is not available: {feature}")]
    Unsupported {
        /// The missing feature.
        feature: String,
    },
    /// An underlying snapshot provider failed. Code `backup.snapshot`.
    #[error("the snapshot provider failed: {reason}")]
    Snapshot {
        /// The provider's reason.
        reason: String,
    },
    /// An underlying I/O operation failed. Code `backup.io`.
    #[error("a backup operation failed underneath the request: {source}")]
    Io {
        /// The underlying error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl BackupError {
    /// The stable code for this error.
    pub fn code(&self) -> &'static str {
        match self {
            BackupError::TargetUnavailable { .. } => "backup.target_unavailable",
            BackupError::SamePhysicalDisk { .. } => "backup.same_physical_disk",
            BackupError::ManifestInvalid { .. } => "backup.manifest_invalid",
            BackupError::VerifyFailed { .. } => "backup.verify_failed",
            BackupError::RetentionRefused { .. } => "backup.retention_refused",
            BackupError::RestoreRefused { .. } => "backup.restore_refused",
            BackupError::NewerSchema { .. } => "backup.newer_schema",
            BackupError::NotFound { .. } => "backup.not_found",
            BackupError::InvariantsFailed { .. } => "backup.invariants_failed",
            BackupError::RestoreFailed { .. } => "backup.restore_failed",
            BackupError::Unsupported { .. } => "backup.unsupported",
            BackupError::Snapshot { .. } => "backup.snapshot",
            BackupError::Io { .. } => "backup.io",
        }
    }

    /// The reason text without the surrounding code.
    pub fn reason(&self) -> String {
        match self {
            BackupError::TargetUnavailable { target, reason } => {
                format!("{target} is not available: {reason}")
            }
            BackupError::SamePhysicalDisk { target, disk } => {
                format!("{target} shares the live data's physical disk {disk}")
            }
            BackupError::ManifestInvalid { reason }
            | BackupError::VerifyFailed { reason }
            | BackupError::RestoreRefused { reason } => reason.clone(),
            BackupError::RetentionRefused { id, reason } => format!("{id}: {reason}"),
            BackupError::NewerSchema {
                backup_schema,
                target_schema,
            } => {
                format!("backup schema {backup_schema} is newer than target schema {target_schema}")
            }
            BackupError::NotFound { id } => format!("no backup {id} exists on the target"),
            BackupError::InvariantsFailed { violations } => violations.clone(),
            BackupError::RestoreFailed { stage, reason } => format!("{stage}: {reason}"),
            BackupError::Unsupported { feature } => feature.clone(),
            BackupError::Snapshot { reason } => reason.clone(),
            BackupError::Io { source } => source.to_string(),
        }
    }

    /// Wraps a message as [`BackupError::Io`].
    pub fn io(message: impl Into<String>) -> Self {
        BackupError::Io {
            source: Box::new(std::io::Error::other(message.into())),
        }
    }
}

impl From<std::io::Error> for BackupError {
    fn from(source: std::io::Error) -> Self {
        BackupError::Io {
            source: Box::new(source),
        }
    }
}

impl From<spot_snapshot::SnapshotError> for BackupError {
    fn from(source: spot_snapshot::SnapshotError) -> Self {
        BackupError::Snapshot {
            reason: source.to_string(),
        }
    }
}

impl From<spot_jobs::Error> for BackupError {
    fn from(source: spot_jobs::Error) -> Self {
        BackupError::io(format!("the job engine failed: {source}"))
    }
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, BackupError>;

/// Renders a list of blockers into the one line a refusal carries.
pub(crate) fn join_blockers(blockers: &[impl fmt::Display]) -> String {
    blockers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_dotted() {
        let samples = [
            BackupError::TargetUnavailable {
                target: "usb".into(),
                reason: "absent".into(),
            },
            BackupError::SamePhysicalDisk {
                target: "usb".into(),
                disk: "sda".into(),
            },
            BackupError::ManifestInvalid {
                reason: "bad".into(),
            },
            BackupError::VerifyFailed {
                reason: "bad".into(),
            },
            BackupError::RestoreRefused {
                reason: "bad".into(),
            },
            BackupError::NewerSchema {
                backup_schema: 2,
                target_schema: 1,
            },
            BackupError::NotFound { id: "b".into() },
            BackupError::InvariantsFailed {
                violations: "v".into(),
            },
            BackupError::RestoreFailed {
                stage: "apply".into(),
                reason: "bad".into(),
            },
            BackupError::Unsupported {
                feature: "cloud".into(),
            },
            BackupError::Snapshot {
                reason: "bad".into(),
            },
            BackupError::Io {
                source: Box::new(std::io::Error::other("bad")),
            },
        ];
        for error in samples {
            assert!(error.code().starts_with("backup."));
            assert!(!error.reason().is_empty());
        }
    }
}
