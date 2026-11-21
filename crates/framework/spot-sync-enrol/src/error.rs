//! Errors and their stable `sync_enrol.*` codes.
//!
//! No message carries a code, a key, a certificate, a hash or a salt. The three ways a one-time
//! code can fail (unknown, expired, already used) are **one** error, so a caller learns nothing
//! about which codes exist.

use std::fmt;

/// The code is unknown, expired or already used, or does not fit the node it names.
pub const CODE_INVALID: &str = "sync_enrol.code_invalid";
/// Too many failed redemptions from one source: locked out for a while.
pub const LOCKED_OUT: &str = "sync_enrol.locked_out";
/// The node already has an identity; revoke it before enrolling again.
pub const NODE_EXISTS: &str = "sync_enrol.node_exists";
/// The node is not known to this tenant.
pub const NODE_UNKNOWN: &str = "sync_enrol.node_unknown";
/// The node is revoked and cannot change again.
pub const NODE_REVOKED: &str = "sync_enrol.node_revoked";
/// The certificate is unreadable, outside its validity (beyond the skew) or not ECDSA P-256.
pub const CERT_INVALID: &str = "sync_enrol.cert_invalid";
/// The certificate was revoked; a revoked fingerprint is never pinned again.
pub const CERT_REVOKED: &str = "sync_enrol.cert_revoked";
/// The proof of possession does not verify.
pub const PROOF_INVALID: &str = "sync_enrol.proof_invalid";
/// The directory's snapshot is too old to trust; it authenticates nobody.
pub const DIRECTORY_STALE: &str = "sync_enrol.directory_stale";
/// The private key file is readable by its group or by others, or is not a plain file.
pub const KEY_FILE_PERMISSIONS: &str = "sync_enrol.key_file_permissions";
/// An argument is malformed.
pub const INVALID_ARGUMENT: &str = "sync_enrol.invalid_argument";
/// The store failed (database, bridge or audit record); nothing was changed.
pub const STORE_FAILED: &str = "sync_enrol.store_failed";
/// The store cannot be built here.
pub const UNSUPPORTED_CONTEXT: &str = "sync_enrol.unsupported_context";
/// The tables could not be created.
pub const PREPARE_FAILED: &str = "sync_enrol.prepare_failed";

/// One enrolment failure.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnrolError {
    /// `sync_enrol.code_invalid`.
    CodeInvalid,
    /// `sync_enrol.locked_out`.
    LockedOut,
    /// `sync_enrol.node_exists`.
    NodeExists,
    /// `sync_enrol.node_unknown`.
    NodeUnknown,
    /// `sync_enrol.node_revoked`.
    NodeRevoked,
    /// `sync_enrol.cert_invalid`.
    CertInvalid,
    /// `sync_enrol.cert_revoked`.
    CertRevoked,
    /// `sync_enrol.proof_invalid`.
    ProofInvalid,
    /// `sync_enrol.directory_stale`.
    DirectoryStale,
    /// `sync_enrol.key_file_permissions`.
    KeyFilePermissions,
    /// `sync_enrol.invalid_argument`.
    InvalidArgument {
        /// A fixed sentence.
        reason: &'static str,
    },
    /// `sync_enrol.store_failed`.
    StoreFailed {
        /// A fixed sentence (with the bridge's code when there is one).
        reason: &'static str,
    },
    /// `sync_enrol.unsupported_context`.
    UnsupportedContext {
        /// A fixed sentence.
        reason: &'static str,
    },
    /// `sync_enrol.prepare_failed`.
    PrepareFailed,
}

impl EnrolError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CodeInvalid => CODE_INVALID,
            Self::LockedOut => LOCKED_OUT,
            Self::NodeExists => NODE_EXISTS,
            Self::NodeUnknown => NODE_UNKNOWN,
            Self::NodeRevoked => NODE_REVOKED,
            Self::CertInvalid => CERT_INVALID,
            Self::CertRevoked => CERT_REVOKED,
            Self::ProofInvalid => PROOF_INVALID,
            Self::DirectoryStale => DIRECTORY_STALE,
            Self::KeyFilePermissions => KEY_FILE_PERMISSIONS,
            Self::InvalidArgument { .. } => INVALID_ARGUMENT,
            Self::StoreFailed { .. } => STORE_FAILED,
            Self::UnsupportedContext { .. } => UNSUPPORTED_CONTEXT,
            Self::PrepareFailed => PREPARE_FAILED,
        }
    }
}

impl fmt::Display for EnrolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::CodeInvalid => write!(f, "{code}: the enrolment code was not accepted"),
            Self::LockedOut => write!(f, "{code}: too many failed attempts, try again later"),
            Self::NodeExists => write!(f, "{code}: the node already has an identity"),
            Self::NodeUnknown => write!(f, "{code}: the node is not known"),
            Self::NodeRevoked => write!(f, "{code}: the node is revoked"),
            Self::CertInvalid => write!(f, "{code}: the certificate is not acceptable"),
            Self::CertRevoked => write!(f, "{code}: the certificate was revoked"),
            Self::ProofInvalid => write!(f, "{code}: the proof of possession does not verify"),
            Self::DirectoryStale => write!(f, "{code}: the node directory is too old to trust"),
            Self::KeyFilePermissions => {
                write!(
                    f,
                    "{code}: the private key file must be readable by its owner only"
                )
            }
            Self::PrepareFailed => write!(f, "{code}: the enrolment tables could not be created"),
            Self::InvalidArgument { reason }
            | Self::StoreFailed { reason }
            | Self::UnsupportedContext { reason } => write!(f, "{code}: {reason}"),
        }
    }
}

impl std::error::Error for EnrolError {}

/// The result of an enrolment call.
pub type Result<T> = std::result::Result<T, EnrolError>;
