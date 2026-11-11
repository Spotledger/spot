//! Typed errors with stable `journal_pii.*` codes.
//!
//! A message names no value: never the plaintext, never the ciphertext, never a nonce and
//! never a key. The detail an error carries is a fixed phrase chosen here, so an error can be
//! logged and enveloped to a client as it stands.

use crate::key::KeyStoreError;

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PiiError {
    /// The personal-field description is unusable.
    #[error("the personal-field description is invalid: {0}")]
    SpecInvalid(&'static str),
    /// The image is not a record image this crate can bind an envelope to.
    #[error("the image is invalid: {0}")]
    ImageInvalid(&'static str),
    /// The data-subject id is missing, empty or not a string.
    #[error("the data-subject id is invalid: {0}")]
    SubjectInvalid(&'static str),
    /// The envelope names a different data subject than the image does at the field's subject path.
    #[error("the personal-field envelope names a data subject other than the record's")]
    SubjectMismatch,
    /// An envelope is not shaped like one, or cannot be opened where it sits.
    #[error("the personal-field envelope is invalid: {0}")]
    EnvelopeInvalid(&'static str),
    /// The envelope carries a version this build does not read.
    #[error(
        "the personal-field envelope at {path} has version {version}, which this build does not read"
    )]
    EnvelopeVersion {
        /// The field path the envelope sits at.
        path: String,
        /// The version marker the envelope carries.
        version: u64,
    },
    /// The field already holds an envelope, so protecting it again would double-encrypt it.
    #[error("the personal field at {path} already holds an envelope")]
    AlreadyProtected {
        /// The field path that already holds an envelope.
        path: String,
    },
    /// No key exists for the data subject of a field being protected.
    #[error("no key exists for the data subject of this field, so its value cannot be protected")]
    KeyMissing,
    /// The key store refused or failed.
    #[error("the subject key store is unavailable: {0}")]
    KeyStore(KeyStoreError),
    /// The nonce source could not produce a nonce.
    #[error("the nonce source is unavailable")]
    NonceUnavailable,
}

impl PiiError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SpecInvalid(_) => "journal_pii.spec_invalid",
            Self::ImageInvalid(_) => "journal_pii.image_invalid",
            Self::SubjectInvalid(_) => "journal_pii.subject_invalid",
            Self::SubjectMismatch => "journal_pii.subject_mismatch",
            Self::EnvelopeInvalid(_) => "journal_pii.envelope_invalid",
            Self::EnvelopeVersion { .. } => "journal_pii.envelope_version",
            Self::AlreadyProtected { .. } => "journal_pii.already_protected",
            Self::KeyMissing => "journal_pii.key_missing",
            Self::KeyStore(_) => "journal_pii.key_store",
            Self::NonceUnavailable => "journal_pii.nonce_unavailable",
        }
    }
}

impl From<KeyStoreError> for PiiError {
    fn from(error: KeyStoreError) -> Self {
        Self::KeyStore(error)
    }
}
