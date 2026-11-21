//! The stable error vocabulary of the Windows system modules (`agent_windows_sys.*`),
//! catalogued in `crates/framework/spot-errors/catalogue/agent_windows_sys.toml`.
//!
//! Every failure a caller can act on is one variant with one dotted code. Messages name
//! volumes, stores, thumbprints and shadow ids — never a secret (the certificate import
//! carries a private key and it never reaches a message or a `Debug` rendering).

/// A VSS operation failed (create, expose, unexpose, delete or volume mapping).
pub const VSS_FAILED: &str = "agent_windows_sys.vss_failed";
/// The named shadow copy is not known to the VSS subsystem.
pub const SHADOW_NOT_FOUND: &str = "agent_windows_sys.shadow_not_found";
/// A volume set was empty: a shadow copy needs at least one volume.
pub const VOLUME_SET_EMPTY: &str = "agent_windows_sys.volume_set_empty";
/// Entering PostgreSQL backup mode failed.
pub const BACKUP_START_FAILED: &str = "agent_windows_sys.backup_start_failed";
/// Leaving PostgreSQL backup mode failed after it had started.
pub const BACKUP_STOP_FAILED: &str = "agent_windows_sys.backup_stop_failed";
/// Backup mode was never entered, so there is nothing to stop.
pub const BACKUP_NOT_ACTIVE: &str = "agent_windows_sys.backup_not_active";
/// Backup mode is already active; a second start would lose the first label.
pub const BACKUP_ALREADY_ACTIVE: &str = "agent_windows_sys.backup_already_active";
/// A thumbprint is not the expected hex form (40 or 64 hex digits).
pub const INVALID_THUMBPRINT: &str = "agent_windows_sys.invalid_thumbprint";
/// A certificate store name is not one of the supported LocalMachine stores.
pub const UNKNOWN_STORE: &str = "agent_windows_sys.unknown_store";
/// The named certificate is not in the store.
pub const CERTIFICATE_NOT_FOUND: &str = "agent_windows_sys.certificate_not_found";
/// An export asked for the private key; only the public part ever leaves the machine.
pub const PRIVATE_KEY_EXPORT_REFUSED: &str = "agent_windows_sys.private_key_export_refused";
/// The certificate is not valid at the requested instant (expired or not yet valid).
pub const CERTIFICATE_NOT_VALID: &str = "agent_windows_sys.certificate_not_valid";
/// The certificate chain does not validate to a trusted root.
pub const CHAIN_INVALID: &str = "agent_windows_sys.chain_invalid";
/// Binding a certificate to the Spotledger HTTPS endpoint failed.
pub const BIND_FAILED: &str = "agent_windows_sys.bind_failed";
/// The VSS adapter cannot restore; the host restores the block image.
pub const RESTORE_UNSUPPORTED: &str = "agent_windows_sys.restore_unsupported";
/// A management operation id could not be built (a static authoring mistake).
pub const INVALID_OPERATION_ID: &str = "agent_windows_sys.invalid_operation_id";
/// An underlying filesystem or process operation failed.
pub const IO: &str = "agent_windows_sys.io";

/// A failure in a Windows system module, with a stable machine-readable code
/// ([`WindowsSysError::code`]).
#[derive(Debug, thiserror::Error)]
pub enum WindowsSysError {
    /// A VSS operation failed; `reason` is a short, secret-free description.
    #[error("the volume shadow copy service refused the operation: {reason}")]
    VssFailed {
        /// What the VSS subsystem reported, in plain words.
        reason: String,
    },
    /// The named shadow copy does not exist (see [`SHADOW_NOT_FOUND`]).
    #[error("no shadow copy with id {id} exists")]
    ShadowNotFound {
        /// The shadow copy id that was looked up.
        id: String,
    },
    /// A volume set carried no volumes (see [`VOLUME_SET_EMPTY`]).
    #[error("a shadow copy needs at least one volume")]
    VolumeSetEmpty,
    /// PostgreSQL backup mode could not be entered (see [`BACKUP_START_FAILED`]).
    #[error("entering PostgreSQL backup mode failed: {reason}")]
    BackupStartFailed {
        /// What the database reported, in plain words.
        reason: String,
    },
    /// PostgreSQL backup mode could not be left (see [`BACKUP_STOP_FAILED`]).
    #[error("leaving PostgreSQL backup mode failed for label {label}: {reason}")]
    BackupStopFailed {
        /// The label `pg_backup_start` returned.
        label: String,
        /// What the database reported, in plain words.
        reason: String,
    },
    /// There is no backup mode to stop (see [`BACKUP_NOT_ACTIVE`]).
    #[error("PostgreSQL is not in backup mode")]
    BackupNotActive,
    /// Backup mode is already active (see [`BACKUP_ALREADY_ACTIVE`]).
    #[error("PostgreSQL is already in backup mode with label {label}")]
    BackupAlreadyActive {
        /// The label of the active backup mode.
        label: String,
    },
    /// A thumbprint is not 40 or 64 hex digits (see [`INVALID_THUMBPRINT`]).
    #[error("the thumbprint {thumbprint} is not 40 or 64 hex digits")]
    InvalidThumbprint {
        /// The refused text (never a secret).
        thumbprint: String,
    },
    /// A store name is not `Root`, `CA` or `My` (see [`UNKNOWN_STORE`]).
    #[error("the certificate store {store} is not one of Root, CA or My")]
    UnknownStore {
        /// The refused store name.
        store: String,
    },
    /// The certificate is absent from the store (see [`CERTIFICATE_NOT_FOUND`]).
    #[error("no certificate with thumbprint {thumbprint} is in the {store} store")]
    CertificateNotFound {
        /// The store that was searched.
        store: String,
        /// The thumbprint that was looked up.
        thumbprint: String,
    },
    /// An export tried to leave with the private key (see [`PRIVATE_KEY_EXPORT_REFUSED`]).
    #[error("refusing to export the private key of certificate {thumbprint}")]
    PrivateKeyExportRefused {
        /// The certificate whose private key was requested.
        thumbprint: String,
    },
    /// The certificate is not valid at the requested instant (see [`CERTIFICATE_NOT_VALID`]).
    #[error("certificate {thumbprint} is not valid at the requested instant: {reason}")]
    CertificateNotValid {
        /// The certificate's thumbprint.
        thumbprint: String,
        /// Why it is not valid (`"expired"` or `"not yet valid"`).
        reason: String,
    },
    /// The certificate chain does not validate (see [`CHAIN_INVALID`]).
    #[error("the chain of certificate {thumbprint} does not validate: {reason}")]
    ChainInvalid {
        /// The certificate's thumbprint.
        thumbprint: String,
        /// Why the chain was refused.
        reason: String,
    },
    /// The HTTPS binding failed (see [`BIND_FAILED`]).
    #[error("binding the certificate to the HTTPS endpoint failed: {reason}")]
    BindFailed {
        /// Why the binding was refused.
        reason: String,
    },
    /// The VSS adapter has no restore path (see [`RESTORE_UNSUPPORTED`]).
    #[error("the VSS snapshot adapter cannot restore: {reason}")]
    RestoreUnsupported {
        /// The remedy the caller should take instead.
        reason: String,
    },
    /// A static operation id failed validation (see [`INVALID_OPERATION_ID`]).
    #[error("the operation id {id} is not valid: {reason}")]
    InvalidOperationId {
        /// The id that failed.
        id: String,
        /// Why it failed.
        reason: String,
    },
    /// An underlying filesystem or process operation failed (see [`IO`]).
    #[error("a windows system module operation failed underneath the request: {source}")]
    Io {
        /// The underlying error, kept for logs and tracing.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl WindowsSysError {
    /// The stable code for this error, for the API error envelope.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            WindowsSysError::VssFailed { .. } => VSS_FAILED,
            WindowsSysError::ShadowNotFound { .. } => SHADOW_NOT_FOUND,
            WindowsSysError::VolumeSetEmpty => VOLUME_SET_EMPTY,
            WindowsSysError::BackupStartFailed { .. } => BACKUP_START_FAILED,
            WindowsSysError::BackupStopFailed { .. } => BACKUP_STOP_FAILED,
            WindowsSysError::BackupNotActive => BACKUP_NOT_ACTIVE,
            WindowsSysError::BackupAlreadyActive { .. } => BACKUP_ALREADY_ACTIVE,
            WindowsSysError::InvalidThumbprint { .. } => INVALID_THUMBPRINT,
            WindowsSysError::UnknownStore { .. } => UNKNOWN_STORE,
            WindowsSysError::CertificateNotFound { .. } => CERTIFICATE_NOT_FOUND,
            WindowsSysError::PrivateKeyExportRefused { .. } => PRIVATE_KEY_EXPORT_REFUSED,
            WindowsSysError::CertificateNotValid { .. } => CERTIFICATE_NOT_VALID,
            WindowsSysError::ChainInvalid { .. } => CHAIN_INVALID,
            WindowsSysError::BindFailed { .. } => BIND_FAILED,
            WindowsSysError::RestoreUnsupported { .. } => RESTORE_UNSUPPORTED,
            WindowsSysError::InvalidOperationId { .. } => INVALID_OPERATION_ID,
            WindowsSysError::Io { .. } => IO,
        }
    }

    /// Wraps a message as [`WindowsSysError::Io`].
    pub fn io(message: impl Into<String>) -> Self {
        WindowsSysError::Io {
            source: Box::new(std::io::Error::other(message.into())),
        }
    }
}

/// The crate's result alias.
pub type WindowsSysResult<T> = Result<T, WindowsSysError>;

impl From<std::io::Error> for WindowsSysError {
    fn from(source: std::io::Error) -> Self {
        WindowsSysError::Io {
            source: Box::new(source),
        }
    }
}
