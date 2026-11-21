//! Signing, the signed manifest's byte form, and the verifying key set with its rotation
//! windows.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::canonical::{decode_signed, put_field, signed_bytes};
use crate::error::ManifestError;
use crate::manifest::Manifest;

/// A manifest and the tenant journal key's Ed25519 signature over its canonical bytes, together
/// with the id of the signing key.
///
/// The signature covers the manifest's canonical bytes **and** the key id (see
/// [`SignedManifest::canonical_bytes`]), so neither can change without the signature failing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    /// The manifest that was signed.
    pub manifest: Manifest,
    /// The id of the signing key (an id, never key material).
    pub key_id: String,
    /// The Ed25519 signature: 64 bytes.
    pub signature: Vec<u8>,
}

impl SignedManifest {
    /// The bytes the signature covers: the manifest's canonical bytes, then the key id.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        signed_bytes(&self.manifest, &self.key_id)
    }

    /// The whole signed manifest as bytes: [`SignedManifest::canonical_bytes`], then the
    /// length-prefixed signature.
    ///
    /// [`SignedManifest::from_bytes`] is its inverse, and the two together are the storage and
    /// wire form when JSON is not wanted.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.canonical_bytes();
        put_field(&mut out, &self.signature);
        out
    }

    /// Decodes [`SignedManifest::to_bytes`], refusing any byte string that is not canonical.
    ///
    /// # Errors
    /// [`ManifestError::FormatInvalid`] when the bytes are truncated, carry trailing data, are not
    /// UTF-8 where text is expected, or would re-encode to different bytes. The manifest's own
    /// invariants and the signature are [`crate::verify`]'s business, not this decoder's.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        let (manifest, key_id, signature) = decode_signed(bytes)?;
        let signed = Self {
            manifest,
            key_id,
            signature,
        };
        if signed.to_bytes() != bytes {
            return Err(ManifestError::FormatInvalid("the bytes are not canonical"));
        }
        Ok(signed)
    }

    /// The generation of the manifest it carries.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }
}

/// Signs `manifest` with `signing_key` under `key_id`.
///
/// The key is the caller's: this crate reads no key material, no randomness and no clock. Signing
/// is the one operation that reads a secret, and the secret never leaves the caller's
/// [`SigningKey`] (nothing here logs or returns it).
///
/// The manifest is not validated here: [`Manifest::validate`] is the model's rule and
/// [`crate::verify`] applies it before it hands back a [`crate::Verified`], so an invalid manifest
/// can be signed but never accepted.
#[must_use]
pub fn sign(manifest: &Manifest, signing_key: &SigningKey, key_id: &str) -> SignedManifest {
    let message = signed_bytes(manifest, key_id);
    let signature = signing_key.sign(&message);
    SignedManifest {
        manifest: manifest.clone(),
        key_id: key_id.to_owned(),
        signature: signature.to_bytes().to_vec(),
    }
}

/// One public key and the manifest generations it is trusted for (inclusive).
#[derive(Clone, Debug)]
struct KeyWindow {
    /// The id the manifests name.
    key_id: String,
    /// The public key.
    key: VerifyingKey,
    /// The first generation the key is trusted for.
    from_generation: u64,
    /// The last generation the key is trusted for.
    to_generation: u64,
}

impl KeyWindow {
    /// Whether the window covers `generation`.
    fn covers(&self, generation: u64) -> bool {
        self.from_generation <= generation && generation <= self.to_generation
    }

    /// Whether the two windows of one key id share a generation.
    fn overlaps(&self, other: &Self) -> bool {
        self.from_generation <= other.to_generation && other.from_generation <= self.to_generation
    }
}

/// The public keys a verifier trusts, each with the manifest generations it is valid for.
///
/// Key rotation is a range, not a switch: an old key stays trusted for the generations it signed,
/// so every manifest an operator kept verifying stays verifiable after a rotation. A key id has
/// exactly one window (overlapping windows of one id would make verification ambiguous), while
/// several key ids may cover the same generation during a rotation overlap.
#[derive(Clone, Debug, Default)]
pub struct VerifyingKeys {
    /// The windows, in the order they were added.
    keys: Vec<KeyWindow>,
}

impl VerifyingKeys {
    /// An empty set, which trusts nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a public key from its 32 bytes, valid for manifest generations
    /// `from_generation..=to_generation`, under the caller-supplied `key_id`.
    ///
    /// # Errors
    /// [`ManifestError::KeyInvalid`] when the bytes are not an Ed25519 public key; otherwise
    /// whatever [`VerifyingKeys::add_key`] reports.
    pub fn add(
        &mut self,
        key_id: &str,
        public_key: &[u8],
        from_generation: u64,
        to_generation: u64,
    ) -> Result<(), ManifestError> {
        let array: [u8; 32] = public_key
            .try_into()
            .map_err(|_| ManifestError::KeyInvalid)?;
        let key = VerifyingKey::from_bytes(&array).map_err(|_| ManifestError::KeyInvalid)?;
        self.add_key(key_id, key, from_generation, to_generation)
    }

    /// Adds a key, valid for manifest generations `from_generation..=to_generation`.
    ///
    /// # Errors
    /// [`ManifestError::KeyWindowInvalid`] when the id is empty, the window is empty
    /// (`to_generation` before `from_generation`) or starts at generation 0, or when the same key
    /// id already has a window that shares a generation with this one.
    pub fn add_key(
        &mut self,
        key_id: &str,
        key: VerifyingKey,
        from_generation: u64,
        to_generation: u64,
    ) -> Result<(), ManifestError> {
        if key_id.trim().is_empty() {
            return Err(ManifestError::KeyWindowInvalid("a key id is empty"));
        }
        if from_generation == 0 {
            return Err(ManifestError::KeyWindowInvalid(
                "a key window starts at generation 1 or later",
            ));
        }
        if to_generation < from_generation {
            return Err(ManifestError::KeyWindowInvalid(
                "a key window ends before it starts",
            ));
        }
        let window = KeyWindow {
            key_id: key_id.to_owned(),
            key,
            from_generation,
            to_generation,
        };
        if self
            .keys
            .iter()
            .any(|other| other.key_id == window.key_id && other.overlaps(&window))
        {
            return Err(ManifestError::KeyWindowInvalid(
                "a key id has one window and this one overlaps it",
            ));
        }
        self.keys.push(window);
        Ok(())
    }

    /// How many windows the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set holds no window.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Checks `signature` over `message` for something of `generation` signed under `key_id`.
    ///
    /// This is the one key-window check: [`crate::verify`] calls it for a manifest, and an
    /// adapter (for example the snapshot signer's key set) calls it for any other message the
    /// tenant journal key signs. The window rule, the three errors and the 64-byte signature rule
    /// are the same for both.
    ///
    /// # Errors
    /// [`ManifestError::KeyUnknown`] when no window names `key_id`,
    /// [`ManifestError::KeyNotValidForGeneration`] when the key exists but not for `generation`,
    /// and [`ManifestError::SignatureInvalid`] when the signature does not verify.
    pub fn verify_message(
        &self,
        key_id: &str,
        generation: u64,
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), ManifestError> {
        let windows: Vec<&KeyWindow> = self
            .keys
            .iter()
            .filter(|window| window.key_id == key_id)
            .collect();
        if windows.is_empty() {
            return Err(ManifestError::KeyUnknown(key_id.to_owned()));
        }
        let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
            return Err(ManifestError::SignatureInvalid);
        };
        let signature = Signature::from_bytes(&bytes);
        let mut covered = false;
        for window in windows {
            if !window.covers(generation) {
                continue;
            }
            covered = true;
            if window.key.verify(message, &signature).is_ok() {
                return Ok(());
            }
        }
        if covered {
            return Err(ManifestError::SignatureInvalid);
        }
        Err(ManifestError::KeyNotValidForGeneration {
            key_id: key_id.to_owned(),
            generation,
        })
    }
}
