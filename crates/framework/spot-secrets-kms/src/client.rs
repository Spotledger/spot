use crate::error::KmsError;
use spot_secrets::SecretBytes;

/// Ciphertext produced by the KMS, with the key version that produced it.
#[derive(Clone, PartialEq, Eq)]
pub struct KmsCiphertext {
    /// The KMS key version that encrypted the value.
    pub key_version: u32,
    /// The ciphertext in the KMS's own wire form (opaque to this crate).
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for KmsCiphertext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KmsCiphertext")
            .field("key_version", &self.key_version)
            .finish_non_exhaustive()
    }
}

/// What the KMS says about the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInfo {
    /// The newest key version; new values are encrypted under it.
    pub latest_version: u32,
    /// The oldest key version that still decrypts.
    pub min_decryption_version: u32,
    /// False when the KMS reports the key as disabled.
    pub enabled: bool,
}

/// A KMS that encrypts and decrypts small values under a key that never leaves it. Everything
/// above this trait works against it, so another KMS protocol is another implementation.
#[async_trait::async_trait]
pub trait KmsClient: Send + Sync {
    /// The KMS key's name (used to build the key id `kms:<name>`).
    fn key_name(&self) -> &str;

    /// Encrypts `plaintext`, binding `aad` as the context.
    async fn encrypt(&self, aad: &[u8], plaintext: &[u8]) -> Result<KmsCiphertext, KmsError>;

    /// Decrypts `ciphertext`, checking `aad`.
    async fn decrypt(
        &self,
        aad: &[u8],
        ciphertext: &KmsCiphertext,
    ) -> Result<SecretBytes, KmsError>;

    /// Reads the key's versions and state.
    async fn key_info(&self) -> Result<KeyInfo, KmsError>;
}
