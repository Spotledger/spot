//! Peer identity keys and handshake signatures (Ed25519, as `spot-agent-enroll` and
//! `spot-bundle-signing` use it).
//!
//! The private half never enters this crate: a side signs through the [`IdentitySigner`] trait,
//! so a hardware-backed key store can keep the key inside its device. The public half is a
//! [`PeerKey`], held as raw bytes and named in errors and logs by its SHA-256 fingerprint.
//! Signatures are deterministic; this crate reads no randomness.

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::ExternalPeerId;
use crate::model::Party;
use crate::util::{decode_hex, hex_lower, sha256};

/// Ed25519 public keys, 32 bytes.
pub const KEY_BYTES: usize = 32;
/// Ed25519 signatures, 64 bytes.
pub const SIGNATURE_BYTES: usize = 64;

/// The public half of a side's identity key.
///
/// Public information: `Debug` prints the fingerprint, not the point encoding.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PeerKey([u8; KEY_BYTES]);

impl PeerKey {
    /// Wraps raw public key bytes, refusing a value that is not a curve point.
    ///
    /// # Errors
    /// [`Error::InvalidParty`] when the bytes are not a valid Ed25519 public key.
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Result<Self> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| Error::InvalidParty {
            reason: "a peer key is not a valid Ed25519 public key",
        })?;
        Ok(Self(bytes))
    }

    /// The raw 32-byte form.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    /// The SHA-256 fingerprint of the key, lower-case hex.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        hex_lower(&sha256(&self.0))
    }

    /// Verifies a signature over `message`.
    ///
    /// # Errors
    /// [`Error::BadSignature`] when it does not verify.
    pub fn verify(&self, message: &[u8], signature: &SignatureBytes) -> Result<()> {
        let key = VerifyingKey::from_bytes(&self.0).map_err(|_| Error::InvalidParty {
            reason: "a peer key is not a valid Ed25519 public key",
        })?;
        key.verify_strict(message, &signature.signature())
            .map_err(|_| Error::BadSignature)
    }
}

impl fmt::Debug for PeerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerKey({})", self.fingerprint())
    }
}

impl TryFrom<String> for PeerKey {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        let bytes = decode_hex::<KEY_BYTES>(&value).ok_or(Error::InvalidParty {
            reason: "a peer key is 64 hexadecimal characters",
        })?;
        Self::from_bytes(bytes)
    }
}

impl From<PeerKey> for String {
    fn from(value: PeerKey) -> Self {
        hex_lower(&value.0)
    }
}

/// An Ed25519 signature, held as the compact 64 bytes the wire carries.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SignatureBytes([u8; SIGNATURE_BYTES]);

impl SignatureBytes {
    /// Wraps raw signature bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; SIGNATURE_BYTES]) -> Self {
        Self(bytes)
    }

    /// The raw 64-byte form.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.0
    }

    fn signature(&self) -> Signature {
        Signature::from_bytes(&self.0)
    }
}

impl fmt::Debug for SignatureBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SignatureBytes({})", hex_lower(&self.0))
    }
}

impl TryFrom<String> for SignatureBytes {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        decode_hex::<SIGNATURE_BYTES>(&value)
            .map(Self)
            .ok_or(Error::BadSignature)
    }
}

impl From<SignatureBytes> for String {
    fn from(value: SignatureBytes) -> Self {
        hex_lower(&value.0)
    }
}

/// The signing capability of one side of a link.
///
/// The handshake asks this for a public key and a signature and never for private material.
/// An adapter over the host agent's key store implements it; [`SeedSigner`] is the software
/// implementation used by tests and by non-agent hosts.
pub trait IdentitySigner {
    /// The party this key belongs to.
    fn party(&self) -> Party;

    /// The public half of the identity key.
    fn public_key(&self) -> PeerKey;

    /// Signs `message` with the identity key.
    fn sign(&self, message: &[u8]) -> SignatureBytes;
}

/// A software Ed25519 identity key over a caller-supplied seed.
///
/// Use this for tests and for hosts that hold their key in software; the seed is the caller's
/// and is never serialised, printed or sent. A host with a hardware key implements
/// [`IdentitySigner`] over that device instead.
pub struct SeedSigner {
    key: SigningKey,
    party: Party,
}

impl SeedSigner {
    /// A signer over a 32-byte seed for `party`.
    #[must_use]
    pub fn from_seed(seed: [u8; KEY_BYTES], party: Party) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
            party,
        }
    }

    /// A signer for a tenant party, for the common test and adapter case.
    ///
    /// # Errors
    /// [`Error::InvalidParty`] when the peer id is malformed.
    pub fn for_external_peer(seed: [u8; KEY_BYTES], peer: impl Into<String>) -> Result<Self> {
        Ok(Self::from_seed(
            seed,
            Party::External(ExternalPeerId::new(peer)?),
        ))
    }
}

impl fmt::Debug for SeedSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeedSigner")
            .field("party", &self.party)
            .field("public_key", &self.public_key())
            .finish()
    }
}

impl IdentitySigner for SeedSigner {
    fn party(&self) -> Party {
        self.party.clone()
    }

    fn public_key(&self) -> PeerKey {
        PeerKey(self.key.verifying_key().to_bytes())
    }

    fn sign(&self, message: &[u8]) -> SignatureBytes {
        SignatureBytes(self.key.sign(message).to_bytes())
    }
}
