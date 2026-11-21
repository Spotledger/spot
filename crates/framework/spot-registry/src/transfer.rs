use crate::{AppName, Digest, Result, SignatureRef};
use sha2::{Digest as _, Sha256};
/// Explicit dual-authorized transfer commitment. Sign its digest using `sign_bundle` with
/// app_id = TRANSFER_APP_ID, app_version = TRANSFER_VERSION, and each publisher's own
/// certificate. This reuses signing's complete chain/anchor/revocation verifier and does
/// not require exposing raw private-key signing APIs. Never accepted as a bundle publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferRequest {
    /// App to transfer.
    pub app: AppName,
    /// Current owner.
    pub from: String,
    /// Recipient.
    pub to: String,
    /// Current ownership generation, preventing replay even after transfer back.
    pub generation: u64,
    /// UTC seconds; both signatures must use this time.
    pub at: i64,
    /// First UTC second at which authorization is no longer valid.
    pub expires_at: i64,
    /// Explanation, included in both signatures.
    pub reason: String,
}
/// Reserved signing identity for transfer authorizations.
pub const TRANSFER_APP_ID: &str = "spotledger-registry-transfer-v1";
/// Transfer protocol version, independent of app releases.
pub const TRANSFER_VERSION: &str = "1.0.0";
impl TransferRequest {
    /// Domain-separated, length-framed commitment authenticated by both publishers.
    pub fn digest(&self) -> Result<Digest> {
        let mut h = Sha256::new();
        h.update(b"spotledger-registry-ownership-transfer-v1\0");
        for field in [
            self.app.as_str(),
            &self.from,
            &self.to,
            &self.generation.to_string(),
            &self.at.to_string(),
            &self.expires_at.to_string(),
            &self.reason,
        ] {
            h.update((field.len() as u64).to_le_bytes());
            h.update(field.as_bytes());
        }
        let hex = h
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        hex.parse().map_err(|_| {
            crate::RegistryError::new("registry.transfer_invalid", "invalid transfer commitment")
        })
    }
}
/// Persisted transfer and both publishers' signed authorization evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferRecord {
    /// Signed request.
    pub request: TransferRequest,
    /// Current owner's signature and certificate.
    pub from: SignatureRef,
    /// Recipient's signature and certificate.
    pub to: SignatureRef,
}
