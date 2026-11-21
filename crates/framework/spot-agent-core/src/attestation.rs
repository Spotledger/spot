//! Hardware-key attestation, mirroring the `spot-deep-access` pattern: a vendor root
//! signs a statement that an administrator's public key is held (only) by a hardware
//! device, and the agent verifies that statement before trusting the key for dangerous
//! operations.
//!
//! The statement is signed over a fixed, deterministic byte encoding of
//! `(operator_key, kind, valid_from, valid_to)`, domain-separated by
//! `SPOT-AGENT-ADMIN-KEY-V1\n`, so a signature made for this statement is useless
//! anywhere else. Vendors are the platform-controlled roots added with
//! [`VendorAttestation::add_root`]; the statement never supplies its own root.
//!
//! This crate defines the trait locally rather than depending on `spot-deep-access`: the
//! pattern is the same, and a host that already has a `KeyAttestation` implementation can
//! adapt it in a few lines. See `## Open questions` in the pull request.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, VerifyingKey};
use spot_signed_command::KeyId;

use crate::error::CoreError;

/// The domain separator of an administrator hardware-key statement.
pub const DOMAIN: &[u8] = b"SPOT-AGENT-ADMIN-KEY-V1\n";

/// A vendor-signed statement that `operator_key` is a hardware-backed administrator key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminAttestation {
    /// The raw administrator public key the statement is about.
    pub operator_key: [u8; 32],
    /// The trusted vendor key id that made the statement.
    pub vendor_key: KeyId,
    /// The key kind; the only accepted value is the literal `hardware`.
    pub kind: String,
    /// Inclusive first valid instant, Unix seconds.
    pub valid_from: i64,
    /// Exclusive last valid instant, Unix seconds.
    pub valid_to: i64,
    /// The raw 64-byte vendor signature over [`AdminAttestation::signing_bytes`].
    pub signature: Vec<u8>,
}

impl AdminAttestation {
    /// The exact bytes the vendor signs: the domain separator then the operator key, a
    /// length-prefixed kind, and the two window instants, all big-endian.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DOMAIN.len() + 32 + 4 + 8 + 16);
        out.extend_from_slice(DOMAIN);
        out.extend_from_slice(&self.operator_key);
        out.extend_from_slice(&(self.kind.len() as u32).to_be_bytes());
        out.extend_from_slice(self.kind.as_bytes());
        out.extend_from_slice(&self.valid_from.to_be_bytes());
        out.extend_from_slice(&self.valid_to.to_be_bytes());
        out
    }

    /// A statement for `public_key`, kind `hardware`, signed by `vendor_key` with
    /// `signature`.
    #[must_use]
    pub fn new(
        public_key: [u8; 32],
        vendor_key: KeyId,
        valid_from: i64,
        valid_to: i64,
        signature: Vec<u8>,
    ) -> Self {
        Self {
            operator_key: public_key,
            vendor_key,
            kind: "hardware".to_owned(),
            valid_from,
            valid_to,
            signature,
        }
    }
}

/// The trusted hardware verification boundary. Implementations authenticate the
/// statement against platform-controlled roots, verify the key binding and its validity
/// window, and reject software keys. A host that has a platform attestation service
/// implements this; [`VendorAttestation`] is the pure local verifier.
pub trait KeyAttestation: Send + Sync {
    /// Verifies `statement` names exactly `expected_key` as a valid hardware key at `now`.
    ///
    /// # Errors
    /// [`CoreError::Attestation`] when the statement is for another key, names a software
    /// key, is outside its window, names an unknown vendor, or its signature does not
    /// verify.
    fn verify(
        &self,
        statement: &AdminAttestation,
        expected_key: &[u8; 32],
        now: i64,
    ) -> Result<(), CoreError>;
}

/// The pure verifier of the documented statement format: a fixed table of platform-trusted
/// vendor roots, each a raw Ed25519 public key.
#[derive(Clone, Debug, Default)]
pub struct VendorAttestation {
    roots: BTreeMap<KeyId, [u8; 32]>,
}

impl VendorAttestation {
    /// An empty verifier: nothing attests until a root is added.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a platform-trusted vendor root and returns its derived key id.
    ///
    /// # Errors
    /// [`CoreError::InvalidKey`] when the bytes are not a valid Ed25519 public key.
    pub fn add_root(&mut self, public_key: &[u8; 32]) -> Result<KeyId, CoreError> {
        VerifyingKey::from_bytes(public_key).map_err(|_| CoreError::InvalidKey)?;
        let id = KeyId::from_public_key(public_key);
        self.roots.insert(id.clone(), *public_key);
        Ok(id)
    }
}

impl KeyAttestation for VendorAttestation {
    fn verify(
        &self,
        statement: &AdminAttestation,
        expected_key: &[u8; 32],
        now: i64,
    ) -> Result<(), CoreError> {
        if &statement.operator_key != expected_key {
            return Err(CoreError::Attestation);
        }
        if statement.kind != "hardware" || now < statement.valid_from || now >= statement.valid_to {
            return Err(CoreError::Attestation);
        }
        let root = self
            .roots
            .get(&statement.vendor_key)
            .ok_or(CoreError::Attestation)?;
        let key = VerifyingKey::from_bytes(root).map_err(|_| CoreError::Attestation)?;
        let signature =
            Signature::from_slice(&statement.signature).map_err(|_| CoreError::Attestation)?;
        key.verify_strict(&statement.signing_bytes(), &signature)
            .map_err(|_| CoreError::Attestation)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    use ed25519_dalek::Signer as _;

    fn vendor_key(seed: u8) -> ed25519_dalek::SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    }

    fn fixture() -> (VendorAttestation, AdminAttestation) {
        let vendor = vendor_key(9);
        let mut verifier = VendorAttestation::new();
        let root = verifier
            .add_root(&vendor.verifying_key().to_bytes())
            .unwrap();
        let operator = vendor_key(3);
        let mut statement = AdminAttestation::new(
            operator.verifying_key().to_bytes(),
            root,
            100,
            1_000,
            Vec::new(),
        );
        statement.signature = vendor.sign(&statement.signing_bytes()).to_bytes().to_vec();
        (verifier, statement)
    }

    #[test]
    fn a_valid_statement_verifies() {
        let (verifier, statement) = fixture();
        verifier
            .verify(&statement, &statement.operator_key, 500)
            .unwrap();
    }

    #[test]
    fn boundaries_and_mismatches_are_refused() {
        let (verifier, statement) = fixture();
        assert!(
            verifier
                .verify(&statement, &statement.operator_key, 100)
                .is_ok()
        );
        assert_eq!(
            verifier.verify(&statement, &statement.operator_key, 1_000),
            Err(CoreError::Attestation)
        );
        let mut software = statement.clone();
        software.kind = "software".to_owned();
        assert_eq!(
            verifier.verify(&software, &software.operator_key, 500),
            Err(CoreError::Attestation)
        );
        assert_eq!(
            verifier.verify(&statement, &[0u8; 32], 500),
            Err(CoreError::Attestation)
        );
        let mut tampered = statement.clone();
        tampered.signature[0] ^= 1;
        assert_eq!(
            verifier.verify(&tampered, &tampered.operator_key, 500),
            Err(CoreError::Attestation)
        );
    }
}
