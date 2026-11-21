//! Keys: public keys and their ids, and key pairs whose secret is never printed.
//!
//! A key id is the first 16 bytes of the SHA-256 of the Ed25519 public key, as lower-case hex
//! (32 characters): the same derivation `spot-signed-command` and `spot-license` use, so one key
//! id means one key everywhere. This crate reads no randomness and no clock: the caller supplies
//! the 32 seed bytes of a key pair.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::SigningError;

/// Bytes of an Ed25519 public key.
pub const PUBLIC_KEY_BYTES: usize = 32;
/// Bytes of an Ed25519 signature.
pub const SIGNATURE_BYTES: usize = 64;

/// The id of a key: 32 lower-case hex characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(String);

impl KeyId {
    /// The id of `key`.
    #[must_use]
    pub fn of(key: &PublicKey) -> Self {
        let digest = Sha256::digest(key.as_bytes());
        let mut hex = String::with_capacity(32);
        for byte in &digest[..16] {
            hex.push_str(&format!("{byte:02x}"));
        }
        Self(hex)
    }

    /// Parses an id from its text form.
    ///
    /// # Errors
    /// [`SigningError::SignatureMalformed`] unless the text is 32 lower-case hex characters.
    pub fn parse(text: &str) -> Result<Self, SigningError> {
        let valid = text.len() == 32
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(SigningError::SignatureMalformed)
        }
    }

    /// The id's text form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An Ed25519 public key. Its bytes were checked to be a curve point when it was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; PUBLIC_KEY_BYTES]);

impl PublicKey {
    /// Builds a public key from its 32 bytes.
    ///
    /// # Errors
    /// [`SigningError::KeyMalformed`] for the wrong length or bytes that are not a curve point.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SigningError> {
        let array =
            <[u8; PUBLIC_KEY_BYTES]>::try_from(bytes).map_err(|_| SigningError::KeyMalformed)?;
        VerifyingKey::from_bytes(&array).map_err(|_| SigningError::KeyMalformed)?;
        Ok(Self(array))
    }

    /// The 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_BYTES] {
        &self.0
    }

    /// The key's id.
    #[must_use]
    pub fn key_id(&self) -> KeyId {
        KeyId::of(self)
    }

    /// Checks `signature` over `message`.
    ///
    /// # Errors
    /// [`SigningError::SignatureInvalid`] when the signature does not verify, or
    /// [`SigningError::KeyMalformed`] if the stored bytes are no longer a point (not reachable
    /// through this type's constructors).
    pub fn verify(
        &self,
        message: &[u8],
        signature: &[u8; SIGNATURE_BYTES],
    ) -> Result<(), SigningError> {
        let key = VerifyingKey::from_bytes(&self.0).map_err(|_| SigningError::KeyMalformed)?;
        let signature = ed25519_dalek::Signature::from_bytes(signature);
        key.verify(message, &signature)
            .map_err(|_| SigningError::SignatureInvalid)
    }
}

/// An Ed25519 key pair. The secret is never printed: `Debug` shows the key id and `<redacted>`.
pub struct KeyPair {
    signing: SigningKey,
    public: PublicKey,
    id: KeyId,
}

/// Builds a key pair from 32 seed bytes the caller drew from a cryptographic source.
#[must_use]
pub fn generate_keypair(seed: &[u8; 32]) -> KeyPair {
    KeyPair::from_seed(seed)
}

impl KeyPair {
    fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let public = PublicKey(signing.verifying_key().to_bytes());
        let id = public.key_id();
        Self {
            signing,
            public,
            id,
        }
    }

    /// The public half.
    #[must_use]
    pub fn public(&self) -> PublicKey {
        self.public
    }

    /// The key's id.
    #[must_use]
    pub fn id(&self) -> &KeyId {
        &self.id
    }

    /// The 32 seed bytes. The only way the secret leaves this type: it is for the caller's
    /// own secure storage, and must never be logged, printed or sent.
    #[must_use]
    pub fn to_seed_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// Signs `message`.
    pub(crate) fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_BYTES] {
        self.signing.sign(message).to_bytes()
    }
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KeyPair {{ key_id: {}, secret: <redacted> }}",
            self.id.as_str()
        )
    }
}
