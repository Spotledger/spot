//! The wire form of the signed command envelope (`MgmtSignedCommand` in the contract):
//!
//! ```json
//! {"command": {"version":1,"id":"..","op":"..","params":{..},"audience":"..","issuer":"<key id>",
//!              "issued_at":0,"expires_at":0,"nonce":"<32 hex>"},
//!  "signatures": [{"key_id":"<32 hex>","signature":"<base64url, no padding, 64 bytes>"}],
//!  "approvals": ["..."]}
//! ```
//!
//! `KeyId` has no public constructor from text, so a key id on the wire is resolved against the
//! trusted administrator keys ([`KeyDirectory`]); an id that is not there is untrusted.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spot_signed_command::command::UncheckedCommand;
use spot_signed_command::{Command, CommandSignature, KeyId, SignedCommand};

/// Why an envelope could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// Not the documented JSON shape.
    #[error("the envelope is malformed")]
    Malformed,
    /// A key id that is not a trusted administrator key.
    #[error("the envelope names a key that is not trusted")]
    UnknownKey,
}

/// The `command` object.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCommand {
    /// Format version (1).
    pub version: u8,
    /// Command id.
    pub id: String,
    /// Operation id.
    pub op: String,
    /// Parameters (string, integer or boolean values).
    pub params: Value,
    /// The site id the command is for.
    pub audience: String,
    /// The issuing key id.
    pub issuer: String,
    /// Issue time, unix seconds.
    pub issued_at: i64,
    /// Expiry, unix seconds.
    pub expires_at: i64,
    /// 16 bytes as 32 lower-case hex characters.
    pub nonce: String,
}

/// One signature.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireSignature {
    /// The signing key id.
    pub key_id: String,
    /// base64url (no padding) of the 64 signature bytes.
    pub signature: String,
}

/// The request body of `POST /mgmt/v1/commands`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireEnvelope {
    /// The command.
    pub command: WireCommand,
    /// Signatures (required for any operation above `read`).
    #[serde(default)]
    pub signatures: Vec<WireSignature>,
    /// Approval records (opaque).
    #[serde(default)]
    pub approvals: Vec<String>,
}

/// Trusted administrator keys by key id text.
pub type KeyDirectory = BTreeMap<String, [u8; 32]>;

/// Parses a request body.
///
/// # Errors
/// [`WireError::Malformed`] for anything but the documented shape.
pub fn decode(bytes: &[u8]) -> Result<WireEnvelope, WireError> {
    serde_json::from_slice(bytes).map_err(|_| WireError::Malformed)
}

/// Encodes a signed command for the wire (what the CLI posts).
///
/// # Errors
/// [`WireError::Malformed`] when the envelope cannot be serialised.
pub fn encode(signed: &SignedCommand, approvals: &[String]) -> Result<String, WireError> {
    let command = &signed.command;
    let mut nonce = String::with_capacity(32);
    for byte in command.nonce() {
        use std::fmt::Write as _;
        let _ = write!(nonce, "{byte:02x}");
    }
    let envelope = WireEnvelope {
        command: WireCommand {
            version: command.version(),
            id: command.id().to_owned(),
            op: command.op().to_owned(),
            params: command.params().clone(),
            audience: command.audience().to_owned(),
            issuer: command.issuer().as_str().to_owned(),
            issued_at: command.issued_at(),
            expires_at: command.expires_at(),
            nonce,
        },
        signatures: signed
            .signatures
            .iter()
            .map(|entry| WireSignature {
                key_id: entry.key_id.as_str().to_owned(),
                signature: URL_SAFE_NO_PAD.encode(entry.signature.to_bytes()),
            })
            .collect(),
        approvals: approvals.to_vec(),
    };
    serde_json::to_string(&envelope).map_err(|_| WireError::Malformed)
}

impl WireEnvelope {
    /// Rebuilds the signed command, resolving key ids through `keys`.
    ///
    /// # Errors
    /// [`WireError::UnknownKey`] for an issuer or signer that is not in `keys`;
    /// [`WireError::Malformed`] for a bad nonce, signature or structure.
    pub fn to_signed(&self, keys: &KeyDirectory) -> Result<SignedCommand, WireError> {
        let wire = &self.command;
        let issuer_key = keys.get(&wire.issuer).ok_or(WireError::UnknownKey)?;
        let nonce = hex16(&wire.nonce).ok_or(WireError::Malformed)?;
        let command = Command::from_parts(UncheckedCommand {
            version: wire.version,
            id: wire.id.clone(),
            op: wire.op.clone(),
            params: wire.params.clone(),
            audience: wire.audience.clone(),
            issuer: KeyId::from_public_key(issuer_key),
            issued_at: wire.issued_at,
            expires_at: wire.expires_at,
            nonce,
        })
        .map_err(|_| WireError::Malformed)?;
        let mut signatures = Vec::with_capacity(self.signatures.len());
        for entry in &self.signatures {
            let key = keys.get(&entry.key_id).ok_or(WireError::UnknownKey)?;
            let bytes = URL_SAFE_NO_PAD
                .decode(entry.signature.as_bytes())
                .map_err(|_| WireError::Malformed)?;
            let array: [u8; 64] = bytes.try_into().map_err(|_| WireError::Malformed)?;
            signatures.push(CommandSignature {
                key_id: KeyId::from_public_key(key),
                signature: Signature::from_bytes(&array),
            });
        }
        Ok(SignedCommand {
            command,
            signatures,
        })
    }
}

fn hex16(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (index, pair) in bytes.chunks(2).enumerate() {
        let high = nibble(*pair.first()?)?;
        let low = nibble(*pair.get(1)?)?;
        *out.get_mut(index)? = (high << 4) | low;
    }
    Some(out)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
