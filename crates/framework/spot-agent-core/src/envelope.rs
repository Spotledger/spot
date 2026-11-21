//! The wire envelope: the canonical command text plus the administrator signatures over
//! it, as plain serde JSON.
//!
//! [`spot_signed_command::SignedCommand`] carries only key **ids** for its signatures and
//! `KeyId` has no public constructor from text, so the envelope carries each signer's raw
//! public key as well. The key id is then re-derived here and must match the declared one,
//! which keeps the id a derived value rather than an attacker-chosen one; the trust store
//! decides whether the key is actually trusted.
//!
//! Shape:
//!
//! ```json
//! {
//!   "command": "<the canonical command JSON, exactly as signed>",
//!   "signatures": [
//!     {"key_id": "<32 lower-case hex>", "public_key": "<base64 32 bytes>", "signature": "<base64 64 bytes>"}
//!   ]
//! }
//! ```
//!
//! [`CommandEnvelope::parse`] treats every byte as hostile: unknown fields, wrong shapes,
//! non-canonical command text and bad key or signature bytes are all refused, and the
//! decoder is fuzzed for panic freedom.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use spot_signed_command::{Command, CommandSignature, KeyId, SignedCommand};

use crate::error::{CoreError, Refusal};

/// One signature in the wire envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeSignature {
    /// The signing key's derived id.
    pub key_id: KeyId,
    /// The signing key's raw Ed25519 public key.
    pub public_key: [u8; 32],
    /// The Ed25519 signature over the signed message.
    pub signature: Signature,
}

impl EnvelopeSignature {
    /// A signature for `public_key` over the signed message of a command. The key id is
    /// derived, not supplied.
    ///
    /// # Errors
    /// [`CoreError::InvalidKey`] when the public key bytes are not a valid Ed25519 point.
    pub fn new(public_key: [u8; 32], signature: Signature) -> Result<Self, CoreError> {
        VerifyingKey::from_bytes(&public_key).map_err(|_| CoreError::InvalidKey)?;
        Ok(Self {
            key_id: KeyId::from_public_key(&public_key),
            public_key,
            signature,
        })
    }

    /// Decodes one wire signature, deriving the key id and demanding it matches.
    fn decode(wire: WireSignature) -> Result<Self, CoreError> {
        if !is_lower_hex(&wire.key_id, 32) {
            return Err(CoreError::Malformed(
                "a signature key id is not 32 hex digits",
            ));
        }
        let public_key = decode_fixed::<32>(&wire.public_key)?;
        let signature_bytes = decode_fixed::<64>(&wire.signature)?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|_| CoreError::Malformed("a signature is not 64 valid bytes"))?;
        let derived = KeyId::from_public_key(&public_key);
        if derived.as_str() != wire.key_id {
            return Err(CoreError::Malformed(
                "the declared key id is not derived from the key",
            ));
        }
        Ok(Self {
            key_id: derived,
            public_key,
            signature,
        })
    }
}

/// The wire envelope: a canonical command and the signatures over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandEnvelope {
    /// The canonical command JSON (exactly the bytes the signatures cover, after the
    /// domain prefix).
    command: String,
    /// The signatures, in the order the console produced them.
    signatures: Vec<EnvelopeSignature>,
}

impl CommandEnvelope {
    /// Builds an envelope from its parts, checking the command text parses as a canonical
    /// command.
    ///
    /// # Errors
    /// [`CoreError::Malformed`] when the command text is not a canonical command.
    pub fn from_parts(
        command: String,
        signatures: Vec<EnvelopeSignature>,
    ) -> Result<Self, CoreError> {
        Command::from_json_str(&command)
            .map_err(|_| CoreError::Malformed("the command text is not a canonical command"))?;
        Ok(Self {
            command,
            signatures,
        })
    }

    /// Parses untrusted wire bytes into an envelope. Every failure is
    /// [`Refusal::EnvelopeMalformed`]; the input is never echoed.
    ///
    /// # Errors
    /// [`Refusal::EnvelopeMalformed`] for anything that is not exactly this shape.
    pub fn parse(bytes: &[u8]) -> Result<Self, Refusal> {
        let wire: WireEnvelope =
            serde_json::from_slice(bytes).map_err(|_| Refusal::EnvelopeMalformed)?;
        Command::from_json_str(&wire.command).map_err(|_| Refusal::EnvelopeMalformed)?;
        let mut signatures: Vec<EnvelopeSignature> = Vec::with_capacity(wire.signatures.len());
        for signature in wire.signatures {
            signatures.push(
                EnvelopeSignature::decode(signature).map_err(|_| Refusal::EnvelopeMalformed)?,
            );
        }
        Ok(Self {
            command: wire.command,
            signatures,
        })
    }

    /// Parses untrusted wire text (a convenience over [`CommandEnvelope::parse`]).
    ///
    /// # Errors
    /// [`Refusal::EnvelopeMalformed`] for anything that is not exactly this shape.
    pub fn parse_str(text: &str) -> Result<Self, Refusal> {
        Self::parse(text.as_bytes())
    }

    /// The canonical command text.
    #[must_use]
    pub fn command_json(&self) -> &str {
        &self.command
    }

    /// The signatures, in the order supplied.
    #[must_use]
    pub fn signatures(&self) -> &[EnvelopeSignature] {
        &self.signatures
    }

    /// The signed command the verifier consumes.
    ///
    /// # Errors
    /// [`Refusal::EnvelopeMalformed`] when the stored command text no longer parses (it
    /// was checked at construction, so this is a defensive check).
    pub fn to_signed(&self) -> Result<SignedCommand, Refusal> {
        let command =
            Command::from_json_str(&self.command).map_err(|_| Refusal::EnvelopeMalformed)?;
        let signatures = self
            .signatures
            .iter()
            .map(|signature| CommandSignature {
                key_id: signature.key_id.clone(),
                signature: signature.signature,
            })
            .collect();
        Ok(SignedCommand {
            command,
            signatures,
        })
    }

    /// Serialises the envelope back to wire JSON.
    ///
    /// # Errors
    /// [`CoreError::Malformed`] when serialisation fails (it cannot for these types, but
    /// the API does not promise an infallible encoding).
    pub fn to_json(&self) -> Result<String, CoreError> {
        let wire = WireEnvelopeOut {
            command: self.command.clone(),
            signatures: self
                .signatures
                .iter()
                .map(|signature| WireSignatureOut {
                    key_id: signature.key_id.as_str().to_owned(),
                    public_key: B64.encode(signature.public_key),
                    signature: B64.encode(signature.signature.to_bytes()),
                })
                .collect(),
        };
        serde_json::to_string(&wire)
            .map_err(|_| CoreError::Malformed("the envelope could not be serialised"))
    }
}

/// The wire shape read from untrusted bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEnvelope {
    command: String,
    signatures: Vec<WireSignature>,
}

/// One wire signature read from untrusted bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSignature {
    key_id: String,
    public_key: String,
    signature: String,
}

/// The wire shape written back out.
#[derive(Serialize)]
struct WireEnvelopeOut {
    command: String,
    signatures: Vec<WireSignatureOut>,
}

/// One wire signature written back out.
#[derive(Serialize)]
struct WireSignatureOut {
    key_id: String,
    public_key: String,
    signature: String,
}

/// Decodes canonical standard base64 into exactly `N` bytes.
fn decode_fixed<const N: usize>(text: &str) -> Result<[u8; N], CoreError> {
    let raw = B64
        .decode(text)
        .map_err(|_| CoreError::Malformed("a base64 field is not base64"))?;
    let array: [u8; N] = raw
        .as_slice()
        .try_into()
        .map_err(|_| CoreError::Malformed("a base64 field has the wrong length"))?;
    if B64.encode(raw.as_slice()) != text {
        return Err(CoreError::Malformed("a base64 field is not canonical"));
    }
    Ok(array)
}

/// Whether `text` is exactly `len` lower-case hex characters.
fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_signed_command::{KeyId, Signer as _, SoftwareSigner};

    use super::*;

    fn command() -> Command {
        Command::new(
            "server.service.restart",
            serde_json::json!({"name": "spot-runtime"}),
            "agent-7",
            KeyId::from_public_key(&[2u8; 32]),
            1_770_000_000,
            1_770_000_900,
        )
    }

    fn envelope() -> (CommandEnvelope, SoftwareSigner) {
        let command = command();
        let signer = SoftwareSigner::from_seed(&[5u8; 32]).unwrap();
        let signature = signer.sign(&command.signed_message().unwrap());
        let signature = EnvelopeSignature::new(signer.public_key_bytes(), signature).unwrap();
        (
            CommandEnvelope::from_parts(
                String::from_utf8(command.canonical_bytes().unwrap()).unwrap(),
                vec![signature],
            )
            .unwrap(),
            signer,
        )
    }

    #[test]
    fn round_trips_through_wire_json() {
        let (envelope, _) = envelope();
        let json = envelope.to_json().unwrap();
        let parsed = CommandEnvelope::parse_str(&json).unwrap();
        assert_eq!(parsed, envelope);
        let signed = parsed.to_signed().unwrap();
        assert_eq!(signed.command.op(), "server.service.restart");
        assert_eq!(signed.signatures.len(), 1);
        assert_eq!(signed.signatures[0].key_id, envelope.signatures()[0].key_id);
    }

    #[test]
    fn a_mismatched_key_id_is_refused() {
        let (envelope, _) = envelope();
        let mut json: serde_json::Value =
            serde_json::from_str(&envelope.to_json().unwrap()).unwrap();
        json["signatures"][0]["key_id"] = serde_json::json!("0".repeat(32));
        let text = serde_json::to_string(&json).unwrap();
        assert_eq!(
            CommandEnvelope::parse_str(&text),
            Err(Refusal::EnvelopeMalformed)
        );
    }

    #[test]
    fn unknown_fields_and_non_canonical_command_are_refused() {
        let (envelope, _) = envelope();
        let mut json: serde_json::Value =
            serde_json::from_str(&envelope.to_json().unwrap()).unwrap();
        json["surprise"] = serde_json::json!(1);
        assert_eq!(
            CommandEnvelope::parse_str(&serde_json::to_string(&json).unwrap()),
            Err(Refusal::EnvelopeMalformed)
        );
        let mut json: serde_json::Value =
            serde_json::from_str(&envelope.to_json().unwrap()).unwrap();
        // A command with whitespace is not canonical.
        json["command"] = serde_json::json!(" { } ");
        assert_eq!(
            CommandEnvelope::parse_str(&serde_json::to_string(&json).unwrap()),
            Err(Refusal::EnvelopeMalformed)
        );
    }
}
