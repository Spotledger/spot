//! Pure verification of vendor-signed hardware-key statements.
use crate::model::{canonical, hash};
use crate::{DeepError, Hash};
use ed25519_dalek::{Signature, VerifyingKey};
use spot_signed_command::KeyId;
use std::collections::BTreeMap;

/// Hardware attestation wire format: the vendor signs canonical JSON of
/// `(vendor_key_id, operator_key, "hardware", valid_from, valid_to)` prefixed by
/// `SPOT-DEEP-ATTESTATION-V1\n`. Vendor roots come from deployment trust, never the operator.
#[derive(Clone, Debug)]
pub struct OperatorAttestation {
    /// Claimed raw operator public key.
    pub operator_key: [u8; 32],
    /// Trusted vendor signing key identifier.
    pub vendor_key: KeyId,
    /// Must equal the literal `hardware`.
    pub kind: String,
    /// Inclusive first instant, Unix seconds.
    pub valid_from: i64,
    /// Exclusive last instant, Unix seconds.
    pub valid_to: i64,
    /// Raw vendor Ed25519 signature.
    pub signature: Vec<u8>,
}
impl OperatorAttestation {
    /// Vendor signature input in the documented statement format.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, DeepError> {
        canonical(
            b"SPOT-DEEP-ATTESTATION-V1\n",
            &(
                self.vendor_key.as_str(),
                self.operator_key,
                &self.kind,
                self.valid_from,
                self.valid_to,
            ),
        )
    }
    /// Commitment to statement and signature.
    pub fn hash(&self) -> Result<Hash, DeepError> {
        canonical(
            b"SPOT-DEEP-ATTESTATION-HASH-V1\n",
            &(self.signing_bytes()?, &self.signature),
        )
        .map(|b| hash(&b))
    }
}
/// Trusted hardware verification boundary, replaceable by a platform attestation service.
/// Implementations must authenticate the statement against platform-controlled roots,
/// verify the key binding and validity window, and reject software keys.
pub trait KeyAttestation {
    /// Verify hardware provenance and the exact expected operator public key.
    fn verify(
        &self,
        statement: &OperatorAttestation,
        expected_key: &[u8; 32],
        now: i64,
    ) -> Result<(), DeepError>;
}
/// Pure verifier of the documented vendor-signed statement format.
#[derive(Default)]
pub struct VendorAttestation {
    roots: BTreeMap<KeyId, [u8; 32]>,
}
impl VendorAttestation {
    /// Add a platform-trusted hardware vendor root.
    pub fn add_root(&mut self, key: [u8; 32]) -> KeyId {
        let id = KeyId::from_public_key(&key);
        self.roots.insert(id.clone(), key);
        id
    }
}
impl KeyAttestation for VendorAttestation {
    fn verify(
        &self,
        statement: &OperatorAttestation,
        expected_key: &[u8; 32],
        now: i64,
    ) -> Result<(), DeepError> {
        if &statement.operator_key != expected_key {
            return Err(DeepError::WrongOperatorKey);
        }
        if statement.kind != "hardware" || now < statement.valid_from || now >= statement.valid_to {
            return Err(DeepError::BadAttestation);
        }
        let key = self
            .roots
            .get(&statement.vendor_key)
            .ok_or(DeepError::BadAttestation)?;
        verify_signature(key, &statement.signing_bytes()?, &statement.signature)
            .map_err(|_| DeepError::BadAttestation)
    }
}
pub(crate) fn verify_signature(
    key: &[u8; 32],
    message: &[u8],
    signature: &[u8],
) -> Result<(), DeepError> {
    let key = VerifyingKey::from_bytes(key).map_err(|_| DeepError::BadSignature)?;
    let signature = Signature::from_slice(signature).map_err(|_| DeepError::BadSignature)?;
    key.verify_strict(message, &signature)
        .map_err(|_| DeepError::BadSignature)
}
