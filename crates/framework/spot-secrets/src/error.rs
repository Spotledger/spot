use thiserror::Error;

/// Failure sealing or opening a secret. Messages never contain plaintext or key material.
#[derive(Debug, Error)]
pub enum SecretError {
    /// The sealed value names a key the provider does not have.
    #[error("secrets.unknown_key: no key {id} version {version}")]
    UnknownKey {
        /// Key id.
        id: String,
        /// Key version.
        version: u32,
    },
    /// The key version was retired, so values still under it can no longer be opened.
    #[error("secrets.retired_key: key {id} version {version} is retired")]
    RetiredKey {
        /// Key id.
        id: String,
        /// Key version.
        version: u32,
    },
    /// Authentication failed: the value was altered or opened with the wrong associated data.
    #[error("secrets.tampered: authentication failed")]
    Tampered,
    /// The sealed value is not in the `SPS1` format.
    #[error("secrets.malformed: {0}")]
    Malformed(&'static str),
    /// The key provider failed (I/O, bad configuration, refused operation).
    #[error("secrets.provider: {0}")]
    Provider(String),
    /// The blind-index key could not be derived from the provider's key-encryption key.
    #[error("secrets.index_key: the blind-index key is unavailable: {0}")]
    IndexKey(String),
    /// A `derive_key` request the provider refused: a fixed sentence, never the `info`, the key or its bytes.
    #[error("secrets.derive_key: {0}")]
    DeriveKey(&'static str),
    /// A blind index names a key version that does not exist (versions start at 1).
    #[error("secrets.unknown_key_version: a blind index names key version {version}")]
    UnknownKeyVersion {
        /// The version byte read from the stored index.
        version: u32,
    },
    /// A name is reserved: it would name an engine-managed column (`<field>__sealed`).
    #[error("secrets.reserved_name: `{0}` uses the reserved `__` infix")]
    ReservedName(String),
}

impl SecretError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownKey { .. } => "secrets.unknown_key",
            Self::RetiredKey { .. } => "secrets.retired_key",
            Self::Tampered => "secrets.tampered",
            Self::Malformed(_) => "secrets.malformed",
            Self::Provider(_) => "secrets.provider",
            Self::IndexKey(_) => "secrets.index_key",
            Self::DeriveKey(_) => "secrets.derive_key",
            Self::UnknownKeyVersion { .. } => "secrets.unknown_key_version",
            Self::ReservedName(_) => "secrets.reserved_name",
        }
    }
}
