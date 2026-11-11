//! Typed errors of the air-gap layer, with stable `airgap.*` codes.
//!
//! Verification fails on the first check that does not hold, and that check's code names the
//! refusal. Callers match on the code, never on the text. Every code has one entry in
//! `crates/framework/spot-errors/catalogue/airgap.toml` with an administrator's remedy.

use spot_bundle_signing::SigningError;
use spot_signed_command::CommandError;
use spot_updater::{UpdaterError, UpdaterErrorCode};

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AirgapError {
    /// The offline manifest (`offline.toml`) does not parse or a field is missing or invalid.
    #[error("the offline manifest is malformed: {detail}")]
    ManifestMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// A signature, key or key-id document does not parse.
    #[error("a signature document is malformed: {detail}")]
    SignatureMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The publisher certificate document does not parse.
    #[error("the publisher certificate is malformed: {detail}")]
    CertificateMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The bundle carries no signature.
    #[error("the offline bundle is not signed")]
    Unsigned,
    /// The certificate or revocation list is issued by a root this store does not trust.
    #[error("the issuing root is not trusted")]
    UnknownRoot,
    /// The issuing root's validity window does not cover the check time.
    #[error("the issuing root is not valid at this time")]
    RootOutsideWindow,
    /// The publisher certificate is not yet valid.
    #[error("the publisher certificate is not yet valid")]
    CertificateNotYetValid,
    /// The publisher certificate has expired.
    #[error("the publisher certificate has expired")]
    CertificateExpired,
    /// The publisher certificate's signature does not verify under its issuer.
    #[error("the publisher certificate's signature does not verify under its issuer")]
    CertificateSignatureInvalid,
    /// The publisher was revoked before the bundle was signed.
    #[error("the publisher was revoked before this bundle was signed")]
    PublisherRevoked,
    /// The signing key was revoked at or before the bundle's signing time.
    #[error("the signing key was revoked before this bundle was signed")]
    KeyRevoked,
    /// The signing key is revoked as compromised; every signature it made is refused.
    #[error("the signing key is revoked as compromised")]
    KeyCompromised,
    /// This exact bundle digest is on the revocation list.
    #[error("this bundle has been revoked")]
    BundleRevoked,
    /// The Ed25519 signature does not verify over the bundle digest and identity.
    #[error("the bundle signature does not verify")]
    SignatureInvalid,
    /// The revocation list's signature does not verify under its issuer.
    #[error("the revocation list's signature does not verify under its issuer")]
    RevocationSignatureInvalid,
    /// The revocation list is older than the one the caller already holds.
    #[error("the revocation list is older than the one already held")]
    RevocationStale {
        /// The generation the caller holds.
        held: u64,
        /// The generation offered.
        offered: u64,
    },
    /// A detached signature names a key the anchor does not trust.
    #[error("the signing key is not trusted: {key_id}")]
    KeyUnknown {
        /// The unknown key id.
        key_id: String,
    },
    /// A detached signature's key window has not opened yet.
    #[error("the signing key is not yet valid: {key_id}")]
    KeyNotYetValid {
        /// The key id.
        key_id: String,
    },
    /// A detached signature's key window has closed.
    #[error("the signing key has expired: {key_id}")]
    KeyExpired {
        /// The key id.
        key_id: String,
    },
    /// A detached signature was refused for a reason the update layer named.
    #[error("the detached signature was refused: {code}")]
    UpdaterRejected {
        /// The update layer's stable code.
        code: &'static str,
    },
    /// The offered version is older than the installed version for the same name and kind.
    #[error("the bundle is a downgrade: {offered} is older than {installed}")]
    Downgrade {
        /// The installed version.
        installed: String,
        /// The offered version.
        offered: String,
    },
    /// The offered version equals the installed one but its digest differs.
    #[error("the bundle {name} {version} is already present with a different digest")]
    DigestChanged {
        /// The bundle name.
        name: String,
        /// The bundle version.
        version: String,
    },
    /// A file the manifest names is missing from the media.
    #[error("a file the manifest names is missing: {path}")]
    FileMissing {
        /// The manifest-relative path.
        path: String,
    },
    /// A file's length is not the manifest entry's.
    #[error("the file {path} is {actual} bytes, not {expected}")]
    FileSizeMismatch {
        /// The manifest-relative path.
        path: String,
        /// The size the manifest claims.
        expected: u64,
        /// The size on disk.
        actual: u64,
    },
    /// A file's SHA-256 is not the manifest entry's.
    #[error("the file {path} has digest {actual}, not {expected}")]
    FileDigestMismatch {
        /// The manifest-relative path.
        path: String,
        /// The digest the manifest claims.
        expected: String,
        /// The digest on disk.
        actual: String,
    },
    /// A file is present on the media that the manifest does not list.
    #[error("a file on the media is not listed in the manifest: {path}")]
    ExtraFile {
        /// The media-relative path.
        path: String,
    },
    /// The repository index (`repo/index.toml`) does not parse.
    #[error("the repository index is malformed: {detail}")]
    RepoIndexMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The repository already holds this name and version with a different digest.
    #[error("the repository already holds {name} {version} with a different digest")]
    RepoConflict {
        /// The bundle name.
        name: String,
        /// The bundle version.
        version: String,
    },
    /// A path on the media could not be read.
    #[error("the media path is unreadable: {path}")]
    MediaUnreadable {
        /// The path that failed.
        path: String,
    },
    /// A staging marker does not parse.
    #[error("the staging marker is malformed: {detail}")]
    StagingMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// A filesystem operation failed.
    #[error("a storage operation failed: {detail}")]
    Io {
        /// What failed, in plain words.
        detail: String,
    },
    /// The OS package mirror manifest does not parse.
    #[error("the package mirror manifest is malformed: {detail}")]
    MirrorMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// A package the mirror manifest names is missing.
    #[error("a package the mirror manifest names is missing: {path}")]
    MirrorMissing {
        /// The manifest-relative path.
        path: String,
    },
    /// A package's SHA-256 is not the mirror manifest's.
    #[error("the package {path} has digest {actual}, not {expected}")]
    MirrorDigestMismatch {
        /// The manifest-relative path.
        path: String,
        /// The digest the manifest claims.
        expected: String,
        /// The digest on disk.
        actual: String,
    },
    /// A package is present that the mirror manifest does not list.
    #[error("a package is not listed in the mirror manifest: {path}")]
    MirrorExtra {
        /// The repository-relative path.
        path: String,
    },
    /// The external mirror metadata builder refused or failed.
    #[error("the mirror metadata builder failed: {detail}")]
    MirrorBuilder {
        /// What failed, in plain words.
        detail: String,
    },
    /// A command bundle document does not parse.
    #[error("the command bundle is malformed: {detail}")]
    CommandBundleMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// A signed command was refused by the verifier.
    #[error("a signed command was refused: {code}")]
    CommandRejected {
        /// The command layer's stable code.
        code: &'static str,
    },
    /// A result bundle document does not parse.
    #[error("the result bundle is malformed: {detail}")]
    ResultMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The result bundle carries no signature.
    #[error("the result bundle is not signed")]
    ResultUnsigned,
    /// The result bundle's agent signature does not verify.
    #[error("the result bundle's signature does not verify")]
    ResultSignatureInvalid,
    /// A result bundle with this id is already recorded with different content.
    #[error("a result bundle with id {id} is already recorded with different content")]
    ResultConflict {
        /// The bundle id.
        id: String,
    },
    /// The audit-chain segment a result bundle carries is malformed.
    #[error("the audit-chain segment is malformed: {detail}")]
    AuditSegmentMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The signing callback refused to sign.
    #[error("the signing callback refused: {detail}")]
    SignerRefused {
        /// Why the callback refused.
        detail: String,
    },
}

impl AirgapError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ManifestMalformed { .. } => "airgap.manifest",
            Self::SignatureMalformed { .. } => "airgap.signature",
            Self::CertificateMalformed { .. } => "airgap.certificate",
            Self::Unsigned => "airgap.unsigned",
            Self::UnknownRoot => "airgap.unknown_root",
            Self::RootOutsideWindow => "airgap.root_outside_window",
            Self::CertificateNotYetValid => "airgap.certificate_not_yet_valid",
            Self::CertificateExpired => "airgap.certificate_expired",
            Self::CertificateSignatureInvalid => "airgap.certificate_signature",
            Self::PublisherRevoked => "airgap.publisher_revoked",
            Self::KeyRevoked => "airgap.key_revoked",
            Self::KeyCompromised => "airgap.key_compromised",
            Self::BundleRevoked => "airgap.bundle_revoked",
            Self::SignatureInvalid => "airgap.signature_invalid",
            Self::RevocationSignatureInvalid => "airgap.revocation_signature",
            Self::RevocationStale { .. } => "airgap.revocation_stale",
            Self::KeyUnknown { .. } => "airgap.key_unknown",
            Self::KeyNotYetValid { .. } => "airgap.key_not_yet_valid",
            Self::KeyExpired { .. } => "airgap.key_expired",
            Self::UpdaterRejected { .. } => "airgap.updater",
            Self::Downgrade { .. } => "airgap.downgrade",
            Self::DigestChanged { .. } => "airgap.digest_changed",
            Self::FileMissing { .. } => "airgap.file_missing",
            Self::FileSizeMismatch { .. } => "airgap.file_size",
            Self::FileDigestMismatch { .. } => "airgap.file_digest",
            Self::ExtraFile { .. } => "airgap.extra_file",
            Self::RepoIndexMalformed { .. } => "airgap.repo_index",
            Self::RepoConflict { .. } => "airgap.repo_conflict",
            Self::MediaUnreadable { .. } => "airgap.media_unreadable",
            Self::StagingMalformed { .. } => "airgap.staging",
            Self::Io { .. } => "airgap.io",
            Self::MirrorMalformed { .. } => "airgap.mirror_manifest",
            Self::MirrorMissing { .. } => "airgap.mirror_missing",
            Self::MirrorDigestMismatch { .. } => "airgap.mirror_digest",
            Self::MirrorExtra { .. } => "airgap.mirror_extra",
            Self::MirrorBuilder { .. } => "airgap.mirror_builder",
            Self::CommandBundleMalformed { .. } => "airgap.command_bundle",
            Self::CommandRejected { .. } => "airgap.command_rejected",
            Self::ResultMalformed { .. } => "airgap.result_bundle",
            Self::ResultUnsigned => "airgap.result_unsigned",
            Self::ResultSignatureInvalid => "airgap.result_signature",
            Self::ResultConflict { .. } => "airgap.result_conflict",
            Self::AuditSegmentMalformed { .. } => "airgap.audit_segment",
            Self::SignerRefused { .. } => "airgap.signer",
        }
    }
}

/// Folds a signing-layer refusal onto the air-gap error with its own stable code.
impl From<SigningError> for AirgapError {
    fn from(error: SigningError) -> Self {
        match error {
            SigningError::UnknownRoot => Self::UnknownRoot,
            SigningError::RootOutsideWindow => Self::RootOutsideWindow,
            SigningError::CertificateNotYetValid => Self::CertificateNotYetValid,
            SigningError::CertificateExpired => Self::CertificateExpired,
            SigningError::CertificateSignatureInvalid => Self::CertificateSignatureInvalid,
            SigningError::PublisherRevoked => Self::PublisherRevoked,
            SigningError::KeyRevoked => Self::KeyRevoked,
            SigningError::KeyCompromised => Self::KeyCompromised,
            SigningError::BundleRevoked => Self::BundleRevoked,
            SigningError::RevocationSignatureInvalid => Self::RevocationSignatureInvalid,
            SigningError::StaleRevocationList => Self::RevocationStale {
                held: 0,
                offered: 0,
            },
            SigningError::DocumentMalformed { detail, .. } => Self::SignatureMalformed { detail },
            SigningError::SignatureMalformed | SigningError::KeyMalformed => {
                Self::SignatureMalformed {
                    detail: "a signature document or key is malformed".to_owned(),
                }
            }
            _ => Self::SignatureInvalid,
        }
    }
}

/// Folds an update-layer refusal onto the air-gap error with its own stable code.
impl From<UpdaterError> for AirgapError {
    fn from(error: UpdaterError) -> Self {
        match error {
            UpdaterError::ManifestMalformed { detail } => Self::ManifestMalformed { detail },
            UpdaterError::Unsigned => Self::Unsigned,
            UpdaterError::KeyUnknown { key_id } => Self::KeyUnknown { key_id },
            UpdaterError::KeyNotYetValid { key_id } => Self::KeyNotYetValid { key_id },
            UpdaterError::KeyExpired { key_id } => Self::KeyExpired { key_id },
            UpdaterError::KeyRevoked { .. } => Self::KeyRevoked,
            UpdaterError::RevocationSignatureInvalid => Self::RevocationSignatureInvalid,
            UpdaterError::RevocationStale { held, offered } => {
                Self::RevocationStale { held, offered }
            }
            UpdaterError::SignatureInvalid => Self::SignatureInvalid,
            UpdaterError::SignatureMalformed { detail } | UpdaterError::KeyMalformed { detail } => {
                Self::SignatureMalformed { detail }
            }
            other => Self::UpdaterRejected {
                code: updater_code(other.code()),
            },
        }
    }
}

/// Folds a signed-command refusal onto the air-gap error, keeping the command layer's code.
impl From<CommandError> for AirgapError {
    fn from(error: CommandError) -> Self {
        Self::CommandRejected { code: error.code() }
    }
}

/// The update layer's stable text for a code.
fn updater_code(code: UpdaterErrorCode) -> &'static str {
    code.as_str()
}
