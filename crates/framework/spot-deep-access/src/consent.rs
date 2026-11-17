//! The wire form of a customer consent and the check the tenant makes when it arrives
//! (WP-SEC-07.T2).
//!
//! A consent travels as two strings: the **record** (this JSON, at most 8192 characters) and the
//! detached **signature** over the receipt's signing bytes. [`check_consent`] runs, at the
//! tenant, the rules of `issue` that need no operator input, so a bad consent is refused when the
//! customer submits it, not later. It does **not** consume the nonce (that is `issue`'s single
//! atomic step, after the attestation) and it is not issuance: nothing is issued without `issue`.

use crate::DeepError;
use crate::attestation::verify_signature;
use crate::issue::scope_subset;
use crate::model::{AccessRequest, ConsentReceipt, CustomerTrust};
use serde::{Deserialize, Serialize};
use spot_iam_access::stepup::AuthMethod;
use spot_signed_command::KeyId;
use spot_support_session::Scope;

/// The longest record accepted.
pub const MAX_RECORD_BYTES: usize = 8192;

/// What the customer signs, before the signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentRecord {
    /// Hash of the request approved (64 lower-case hex characters).
    pub request_hash: String,
    /// The approving identity.
    pub approver: String,
    /// The approver's Ed25519 public key (64 lower-case hex characters); the key id is derived.
    pub approver_public_key: String,
    /// How the approver authenticated.
    pub method: AuthMethod,
    /// The scope approved (never wider than requested).
    pub approved_scope: Scope,
    /// Start of validity (Unix seconds).
    pub valid_from: i64,
    /// End of validity (exclusive).
    pub valid_to: i64,
    /// The single-use receipt nonce (32 lower-case hex characters).
    pub nonce: String,
}

fn from_hex<const N: usize>(text: &str) -> Result<[u8; N], DeepError> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return Err(DeepError::InvalidRequest);
    }
    let mut out = [0_u8; N];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let digit = |b: u8| match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(DeepError::InvalidRequest),
        };
        let (hi, lo) = (digit(pair[0])?, digit(pair[1])?);
        out[i] = hi * 16 + lo;
    }
    Ok(out)
}

/// Lower-case hex of `bytes`.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl ConsentRecord {
    /// Parses the record text.
    ///
    /// # Errors
    /// [`DeepError::InvalidRequest`] for text over [`MAX_RECORD_BYTES`], unknown fields or bad
    /// JSON.
    pub fn from_json(text: &str) -> Result<Self, DeepError> {
        if text.len() > MAX_RECORD_BYTES {
            return Err(DeepError::InvalidRequest);
        }
        serde_json::from_str(text).map_err(|_| DeepError::InvalidRequest)
    }

    /// The receipt this record and `signature` make. The key id is derived from the public key,
    /// so a record cannot name a key id that is not its own.
    ///
    /// # Errors
    /// [`DeepError::InvalidRequest`] for a malformed hash, key or nonce.
    pub fn into_receipt(self, signature: Vec<u8>) -> Result<ConsentReceipt, DeepError> {
        let public_key: [u8; 32] = from_hex(&self.approver_public_key)?;
        Ok(ConsentReceipt {
            request_hash: from_hex(&self.request_hash)?,
            approver: self.approver,
            approver_key: KeyId::from_public_key(&public_key),
            method: self.method,
            approved_scope: self.approved_scope,
            valid_from: self.valid_from,
            valid_to: self.valid_to,
            nonce: from_hex(&self.nonce)?,
            signature,
        })
    }
}

/// Checks a consent against its request and the customer's trust list: the right request, an
/// enrolled key of that identity, not the requester, the requesting operator or the operator's
/// key, a valid signature, inside its window, and no wider than requested.
///
/// # Errors
/// The [`DeepError`] naming the first rule that failed.
pub fn check_consent(
    request: &AccessRequest,
    receipt: &ConsentReceipt,
    trust: &CustomerTrust,
    operator_key: Option<&[u8; 32]>,
    now: i64,
) -> Result<(), DeepError> {
    if receipt.request_hash != request.hash()? {
        return Err(DeepError::WrongRequest);
    }
    let trusted = trust
        .get(&request.spec.tenant, &receipt.approver_key)
        .ok_or(DeepError::UnknownApprover)?;
    if trusted.identity != receipt.approver {
        return Err(DeepError::UnknownApprover);
    }
    if receipt.approver == request.requester.id
        || receipt.approver == request.spec.requested_by
        || operator_key.is_some_and(|key| trusted.public_key == *key)
    {
        return Err(DeepError::OperatorApproval);
    }
    verify_signature(
        &trusted.public_key,
        &receipt.signing_bytes()?,
        &receipt.signature,
    )?;
    if now < receipt.valid_from {
        return Err(DeepError::NotYetValid);
    }
    if now >= receipt.valid_to {
        return Err(DeepError::ExpiredReceipt);
    }
    if !scope_subset(&receipt.approved_scope, &request.spec.scope) {
        return Err(DeepError::WidenedScope);
    }
    Ok(())
}
