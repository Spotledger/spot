use spot_secrets::SecretError;
use thiserror::Error;

/// A KMS failure. Every message is a fixed sentence: never the token, a ciphertext, a key, a
/// response body or an address.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum KmsError {
    /// The configuration is not usable.
    #[error("secrets_kms.config_invalid: {0}")]
    ConfigInvalid(&'static str),
    /// The token file cannot be used (missing, wrong type, too open, empty, too large).
    #[error("secrets_kms.token_unreadable: {0}")]
    TokenUnreadable(&'static str),
    /// The egress policy refused the KMS address.
    #[error("secrets_kms.egress_refused: the egress policy refused the KMS address")]
    EgressRefused,
    /// The KMS could not be reached or did not answer in time.
    #[error("secrets_kms.unreachable: {0}")]
    Unreachable(&'static str),
    /// The KMS answered with a non-success status.
    #[error("secrets_kms.rejected: the KMS answered HTTP {status}")]
    Rejected {
        /// The HTTP status code.
        status: u16,
    },
    /// The KMS answered in a shape this client does not accept.
    #[error("secrets_kms.response_invalid: {0}")]
    ResponseInvalid(&'static str),
    /// The key is disabled in the KMS.
    #[error("secrets_kms.key_disabled: the key is disabled in the KMS")]
    KeyDisabled,
    /// The key version is below the KMS's minimum decryption version.
    #[error("secrets_kms.version_retired: key version {version} is retired in the KMS")]
    VersionRetired {
        /// The retired version.
        version: u32,
    },
    /// A caller's input is outside the accepted bounds.
    #[error("secrets_kms.input_invalid: {0}")]
    InputInvalid(&'static str),
    /// The seed file is corrupt, foreign or otherwise not acceptable.
    #[error("secrets_kms.seed_invalid: {0}")]
    SeedInvalid(&'static str),
    /// The seed file could not be read or written.
    #[error("secrets_kms.seed_io: {0}")]
    SeedIo(&'static str),
}

impl KmsError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid(_) => "secrets_kms.config_invalid",
            Self::TokenUnreadable(_) => "secrets_kms.token_unreadable",
            Self::EgressRefused => "secrets_kms.egress_refused",
            Self::Unreachable(_) => "secrets_kms.unreachable",
            Self::Rejected { .. } => "secrets_kms.rejected",
            Self::ResponseInvalid(_) => "secrets_kms.response_invalid",
            Self::KeyDisabled => "secrets_kms.key_disabled",
            Self::VersionRetired { .. } => "secrets_kms.version_retired",
            Self::InputInvalid(_) => "secrets_kms.input_invalid",
            Self::SeedInvalid(_) => "secrets_kms.seed_invalid",
            Self::SeedIo(_) => "secrets_kms.seed_io",
        }
    }

    /// The refusal a `KeyProvider` returns: a `SecretError::Provider` whose text names the key
    /// and carries this error's code and fixed sentence.
    pub(crate) fn into_secret(self, key_name: &str) -> SecretError {
        SecretError::Provider(format!("{self} (key {key_name})"))
    }
}
