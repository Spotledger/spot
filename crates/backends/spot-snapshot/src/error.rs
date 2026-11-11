//! Stable error codes for snapshot and restore operations, matching the `snapshot.*`
//! family of the native API error registry (ADR-055). Catalogued in
//! `crates/framework/spot-errors/catalogue/snapshot.toml`.

/// A snapshot or restore failure with a stable machine-readable code.
///
/// The codes never change: callers match on [`SnapshotError::code`], not on the
/// message. Messages name paths and reasons but never a secret (the brief's rule;
/// the child environment carries credentials, never the command line).
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// PostgreSQL major version differs between manifest and restore target (only
    /// major 18 is supported). Code `snapshot.version_mismatch`.
    #[error(
        "snapshot was taken on PostgreSQL {snapshot_major} but the target runs {target_major}; a physical restore cannot change major versions"
    )]
    VersionMismatch {
        /// Major version recorded in the snapshot manifest.
        snapshot_major: u32,
        /// Major version observed at the restore target.
        target_major: u32,
    },
    /// The restore would overwrite existing data without explicit consent. Code
    /// `snapshot.replace_required`.
    #[error(
        "the restore target already holds data; pass replace with a confirmation token to move it aside first"
    )]
    ReplaceRequired {
        /// What already exists (a database name or a data directory path).
        target: String,
    },
    /// The caller asked for a replace but the token did not match the target. Code
    /// `snapshot.replace_token_mismatch`.
    #[error("the confirmation token does not match the replace target: {expected}")]
    ReplaceTokenMismatch {
        /// The token the caller had to repeat (derived from the target, shown here, not a secret).
        expected: String,
    },
    /// A required tool (pg_dump, pg_restore, pg_ctl) is missing. Code `snapshot.tool_missing`.
    #[error("the required tool {tool} was not found in {searched}")]
    ToolMissing {
        /// The tool's file name, for example `pg_dump`.
        tool: String,
        /// Directories that were searched.
        searched: String,
    },
    /// A byte of the snapshot changed since the manifest recorded it. Code
    /// `snapshot.verify_failed`.
    #[error("snapshot verification failed: {reason}")]
    VerifyFailed {
        /// Plain-language reason naming the offending file.
        reason: String,
    },
    /// A restore ran but the invariant checks failed. Code `snapshot.invariants_failed`.
    #[error("restore completed but invariants failed: {violations}")]
    InvariantsFailed {
        /// One line per violation, joined; the structured list is in the step result.
        violations: String,
    },
    /// A drop was refused because the database or directory is not a scratch the caller may
    /// destroy: it carries no (or another) scratch marker, is the live database, or lies
    /// outside the scratch root. Code `snapshot.not_scratch`.
    #[error("`{target}` is not a scratch the snapshot crate may drop: {reason}")]
    NotScratch {
        /// The database name or directory path that was refused.
        target: String,
        /// Why it was refused.
        reason: String,
    },
    /// An underlying operation (io, database, child process) failed. Code `snapshot.io`.
    #[error("snapshot operation failed underneath the request: {source}")]
    Io {
        /// The underlying error, kept for logs and tracing.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl SnapshotError {
    /// The stable code for this error, for the API error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            SnapshotError::VersionMismatch { .. } => "snapshot.version_mismatch",
            SnapshotError::ReplaceRequired { .. } => "snapshot.replace_required",
            SnapshotError::ReplaceTokenMismatch { .. } => "snapshot.replace_token_mismatch",
            SnapshotError::ToolMissing { .. } => "snapshot.tool_missing",
            SnapshotError::VerifyFailed { .. } => "snapshot.verify_failed",
            SnapshotError::InvariantsFailed { .. } => "snapshot.invariants_failed",
            SnapshotError::NotScratch { .. } => "snapshot.not_scratch",
            SnapshotError::Io { .. } => "snapshot.io",
        }
    }

    /// The reason text (the human-facing part of the error).
    pub fn reason(&self) -> String {
        match self {
            SnapshotError::VersionMismatch {
                snapshot_major,
                target_major,
            } => format!(
                "snapshot major version {snapshot_major} differs from target major version {target_major}"
            ),
            SnapshotError::ReplaceRequired { target } => {
                format!("the target {target} already holds data")
            }
            SnapshotError::ReplaceTokenMismatch { expected } => {
                format!("the confirmation token must be {expected}")
            }
            SnapshotError::ToolMissing { tool, searched } => {
                format!("the tool {tool} was not found in {searched}")
            }
            SnapshotError::VerifyFailed { reason } => reason.clone(),
            SnapshotError::InvariantsFailed { violations } => violations.clone(),
            SnapshotError::NotScratch { target, reason } => {
                format!("{target} is not a scratch: {reason}")
            }
            SnapshotError::Io { source } => source.to_string(),
        }
    }

    /// Wraps a message as [`SnapshotError::Io`] (an `io::Error` of the `other` kind).
    pub fn io(message: impl Into<String>) -> Self {
        SnapshotError::Io {
            source: Box::new(std::io::Error::other(message.into())),
        }
    }
}

/// The crate's result alias.
pub type SnapshotResult<T> = Result<T, SnapshotError>;

impl From<std::io::Error> for SnapshotError {
    fn from(source: std::io::Error) -> Self {
        SnapshotError::Io {
            source: Box::new(source),
        }
    }
}
